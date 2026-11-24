mod capture;
mod focus;
mod input;
mod map;

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;

use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UILegacyIAccessiblePattern, UIRangeValuePattern,
    UIScrollItemPattern, UIScrollPattern, UISelectionItemPattern, UITogglePattern, UIValuePattern,
    UIWindowPattern,
};
use uiautomation::types::{
    ControlType, ExpandCollapseState, Handle, Point, ScrollAmount, ToggleState,
    WindowInteractionState,
};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

use super::{
    ActionKind, ActionRequest, ActionResult, BackendError, BackendErrorCode, Delivery,
    ObserveRequest, RootFilters, RootInfo, RootKind, RootObservation, matches_root_filters,
};
use crate::outline::{Frame, UiNode, canonical_role, is_text_sparse};

use self::map::{
    PatternSupport, actions_for_patterns, frame_from_uia_rect, role_for_control_type,
    root_kind_for_control_type,
};
use focus::{CursorGuard, ForegroundGuard};

const MAX_DEPTH: usize = 18;
const MAX_NODES: usize = 3_000;
const MAX_CHILDREN_PER_NODE: usize = 500;
const PROCESS_PATH_CAPACITY: usize = 32_768;

thread_local! {
    static AUTOMATION: RefCell<Option<UIAutomation>> = const { RefCell::new(None) };
}

pub fn observe(root: &RootInfo, request: ObserveRequest) -> Result<RootObservation, BackendError> {
    let mut live_root = None;
    let tree = if request.semantic {
        let opened = open_root(
            root,
            BackendErrorCode::ObservationFailed,
            "opening the UIAutomation root for observation",
        )?;
        let tree = observe_tree(&opened, root)?;
        live_root = Some(opened);
        tree
    } else {
        UiNode {
            role: root.kind.to_string(),
            title: root.title.clone(),
            frame: root.frame,
            enabled: true,
            ..UiNode::default()
        }
    };
    let text_sparse = is_text_sparse(&tree);
    let screenshot = if request.capture.should_capture(text_sparse) {
        Some(match capture::capture_window(root) {
            Ok(png) => png,
            Err(error) => {
                log::debug!(
                    "PrintWindow capture failed for root {}; falling back to UIAutomation: {error}",
                    root.ref_id
                );
                if live_root.is_none() {
                    live_root = Some(open_root(
                        root,
                        BackendErrorCode::CaptureFailed,
                        "opening the UIAutomation root for capture",
                    )?);
                }
                let opened = live_root.as_ref().ok_or_else(|| {
                    backend_error(
                        BackendErrorCode::CaptureFailed,
                        "the UIAutomation capture root was unexpectedly unavailable",
                    )
                })?;
                capture::capture_element(&opened.element)?
            }
        })
    } else {
        None
    };
    Ok(RootObservation {
        root: root.clone(),
        tree,
        text_sparse,
        screenshot,
        screenshot_mime: "image/png",
    })
}

pub fn perform_action(
    root: &RootInfo,
    request: &ActionRequest,
) -> Result<ActionResult, BackendError> {
    match request.kind {
        ActionKind::Press | ActionKind::Click => {
            if request.kind == ActionKind::Press
                || (request.target_path.is_some()
                    && request.button == super::MouseButton::Left
                    && request.click_count == 1)
            {
                let target = target(root, request)?;
                return Ok(match target.press() {
                    Ok(message) => ActionResult::worked(message, Delivery::Ax),
                    Err(uia_error) => {
                        let live_frame = target.frame();
                        let frame = live_frame
                            .has_area()
                            .then_some(live_frame)
                            .or_else(|| request.target_frame.filter(|frame| frame.has_area()));
                        let Some(frame) = frame else {
                            return Ok(ActionResult::didnt(
                                format!("{uia_error}; the target has no clickable frame"),
                                Delivery::None,
                            ));
                        };
                        let (x, y) = frame.center();
                        let _cursor_guard = CursorGuard::acquire();
                        let _foreground_guard = ForegroundGuard::acquire(root);
                        input::click(x, y, super::MouseButton::Left, 1)?;
                        ActionResult::unknown(
                            format!("{uia_error}; uiautomation mouse events were posted instead"),
                            Delivery::ForegroundHid,
                        )
                    }
                });
            }
            let (x, y) = action_point(root, request)?;
            let _cursor_guard = CursorGuard::acquire();
            let _foreground_guard = ForegroundGuard::acquire(root);
            input::click(x, y, request.button, request.click_count)?;
            Ok(ActionResult::unknown(
                "uiautomation mouse events were posted",
                Delivery::ForegroundHid,
            ))
        }
        ActionKind::SetText => {
            let text = request.text.as_deref().ok_or_else(|| {
                BackendError::new(BackendErrorCode::InvalidAction, "set_text requires text")
            })?;
            let target = target(root, request)?;
            match target.set_text(text) {
                Ok(message) => Ok(ActionResult::worked(message, Delivery::Ax)),
                Err(uia_error) => {
                    let focus_failed = target.focus().is_err();
                    let click_point = if focus_failed {
                        let frame = target.frame();
                        if !frame.has_area() {
                            return Ok(ActionResult::didnt(
                                format!(
                                    "{uia_error}; the target also rejected focus and has no clickable frame"
                                ),
                                Delivery::None,
                            ));
                        }
                        Some(frame.center())
                    } else {
                        None
                    };
                    let _cursor_guard = click_point.map(|_| CursorGuard::acquire());
                    let _foreground_guard = ForegroundGuard::acquire(root);
                    if let Some((x, y)) = click_point {
                        input::click(x, y, super::MouseButton::Left, 1)?;
                    }
                    input::keypress(&["ctrl+a".into()])?;
                    input::type_text(text)?;
                    Ok(ActionResult::unknown(
                        format!(
                            "{uia_error}; uiautomation keyboard replacement events were posted instead"
                        ),
                        Delivery::ForegroundHid,
                    ))
                }
            }
        }
        ActionKind::TypeText => {
            let text = request.text.as_deref().ok_or_else(|| {
                BackendError::new(BackendErrorCode::InvalidAction, "type_text requires text")
            })?;
            let click_point = if request.target_path.is_some() {
                let target = target(root, request)?;
                if target.focus().is_err() {
                    let frame = target.frame();
                    if !frame.has_area() {
                        return Ok(ActionResult::didnt(
                            "target rejected focus and has no clickable frame",
                            Delivery::None,
                        ));
                    }
                    Some(frame.center())
                } else {
                    None
                }
            } else {
                None
            };
            let _cursor_guard = click_point.map(|_| CursorGuard::acquire());
            let _foreground_guard = ForegroundGuard::acquire(root);
            if let Some((x, y)) = click_point {
                input::click(x, y, super::MouseButton::Left, 1)?;
            }
            input::type_text(text)?;
            Ok(ActionResult::unknown(
                "uiautomation Unicode keyboard events were posted",
                Delivery::ForegroundHid,
            ))
        }
        ActionKind::Keypress => {
            let keys = request.keys.as_deref().ok_or_else(|| {
                BackendError::new(BackendErrorCode::InvalidAction, "keypress requires keys")
            })?;
            let click_point = if request.target_path.is_some() {
                let target = target(root, request)?;
                if target.focus().is_err() {
                    let frame = target.frame();
                    if !frame.has_area() {
                        return Ok(ActionResult::didnt(
                            "keypress target rejected focus and has no clickable frame",
                            Delivery::None,
                        ));
                    }
                    Some(frame.center())
                } else {
                    None
                }
            } else {
                None
            };
            let _cursor_guard = click_point.map(|_| CursorGuard::acquire());
            let _foreground_guard = ForegroundGuard::acquire(root);
            if let Some((x, y)) = click_point {
                input::click(x, y, super::MouseButton::Left, 1)?;
            }
            input::keypress(keys)?;
            Ok(ActionResult::unknown(
                "uiautomation keyboard events were posted",
                Delivery::ForegroundHid,
            ))
        }
        ActionKind::Scroll => {
            let scroll_x = request.scroll_x.unwrap_or(0.0);
            let scroll_y = request.scroll_y.unwrap_or(0.0);
            validate_scroll_deltas(scroll_x, scroll_y)?;
            if scroll_x == 0.0 && scroll_y == 0.0 {
                return Ok(ActionResult::worked(
                    "scroll deltas were zero; no action was needed",
                    Delivery::None,
                ));
            }
            let target = scroll_target(root, request)?;
            match target.scroll(scroll_x, scroll_y) {
                Ok(message) => Ok(ActionResult::worked(message, Delivery::Ax)),
                Err(uia_error) => {
                    let frame = target.frame();
                    let focus_failed = target.focus().is_err();
                    let mouse_action = if focus_failed {
                        if !frame.has_area() {
                            return Ok(ActionResult::didnt(
                                format!(
                                    "{uia_error}; the target rejected focus and has no frame for keyboard fallback"
                                ),
                                Delivery::None,
                            ));
                        }
                        Some((frame.center(), true))
                    } else if frame.has_area() {
                        Some((frame.center(), false))
                    } else {
                        None
                    };
                    let _cursor_guard = mouse_action.map(|_| CursorGuard::acquire());
                    let _foreground_guard = ForegroundGuard::acquire(root);
                    if let Some(((x, y), should_click)) = mouse_action {
                        if should_click {
                            input::click(x, y, super::MouseButton::Left, 1)?;
                        } else {
                            input::move_mouse(x, y)?;
                        }
                    }
                    input::scroll_with_keyboard(scroll_x, scroll_y)?;
                    Ok(ActionResult::unknown(
                        format!(
                            "{uia_error}; uiautomation keyboard scroll events were posted instead"
                        ),
                        Delivery::ForegroundHid,
                    ))
                }
            }
        }
        ActionKind::Drag => {
            let path = request.path.as_deref().ok_or_else(|| {
                BackendError::new(BackendErrorCode::InvalidAction, "drag requires a path")
            })?;
            let _cursor_guard = CursorGuard::acquire();
            let _foreground_guard = ForegroundGuard::acquire(root);
            input::drag(path, request.button)?;
            Ok(ActionResult::unknown(
                "uiautomation mouse drag events were posted",
                Delivery::ForegroundHid,
            ))
        }
        ActionKind::MoveMouse => {
            let (x, y) = action_point(root, request)?;
            let _foreground_guard = ForegroundGuard::acquire(root);
            input::move_mouse(x, y)?;
            Ok(ActionResult::unknown(
                "a uiautomation mouse-move event was posted",
                Delivery::ForegroundHid,
            ))
        }
    }
}

pub fn list_roots(filters: &RootFilters) -> Result<Vec<RootInfo>, BackendError> {
    let automation = create_automation(
        BackendErrorCode::ObservationFailed,
        "initializing uiautomation for root enumeration",
    )?;
    let desktop = automation.get_root_element().map_err(|error| {
        backend_error(
            BackendErrorCode::ObservationFailed,
            format!("uiautomation get_root_element failed: {error}"),
        )
    })?;
    let walker = automation.get_control_view_walker().map_err(|error| {
        backend_error(
            BackendErrorCode::ObservationFailed,
            format!("uiautomation get_control_view_walker failed: {error}"),
        )
    })?;

    let mut roots = Vec::new();
    for element in walker.get_children(&desktop).unwrap_or_default() {
        if element.is_offscreen().unwrap_or(true) {
            continue;
        }
        let Ok(control_type) = element.get_control_type() else {
            continue;
        };
        let localized_control_type = element.get_localized_control_type().unwrap_or_default();
        let class_name = element.get_classname().unwrap_or_default();
        let Some(kind) =
            root_kind_for_element(&element, control_type, &localized_control_type, &class_name)
        else {
            continue;
        };
        let pid = element.get_process_id().unwrap_or_default();
        if pid == 0 {
            continue;
        }
        let frame = element_frame(&element);
        if !frame.has_area() {
            continue;
        }
        let Some(window_id) = element_window_id(&element) else {
            continue;
        };
        let executable_name = process_executable_name(pid).unwrap_or_default();
        let root = RootInfo {
            ref_id: String::new(),
            app_name: app_display_name(&executable_name, &class_name),
            bundle_id: executable_name,
            pid,
            title: element.get_name().unwrap_or_default(),
            kind,
            window_id,
            frame,
        };
        if matches_root_filters(&root, filters) {
            roots.push(root);
        }
    }
    // The crate does not expose Win32 z-order. Preserve the control-view
    // sibling order returned by its walker, which is stable for a snapshot.
    Ok(roots)
}

fn root_kind_for_element(
    element: &UIElement,
    control_type: ControlType,
    localized_control_type: &str,
    class_name: &str,
) -> Option<RootKind> {
    let mut kind =
        root_kind_for_control_type(control_type as i32, localized_control_type, class_name)?;
    if let Ok(pattern) = element.get_pattern::<UIWindowPattern>() {
        if pattern.get_window_interaction_state().ok() == Some(WindowInteractionState::Closing) {
            return None;
        }
        if pattern.is_modal().unwrap_or(false) {
            kind = RootKind::Dialog;
        }
    }
    if element.is_dialog().unwrap_or(false) {
        kind = RootKind::Dialog;
    }
    Some(kind)
}

struct AutomationRoot {
    automation: UIAutomation,
    walker: UITreeWalker,
    element: UIElement,
}

fn open_root(
    root: &RootInfo,
    code: BackendErrorCode,
    operation: &str,
) -> Result<AutomationRoot, BackendError> {
    let automation = create_automation(code, operation)?;
    let walker = automation.get_control_view_walker().map_err(|error| {
        backend_error(
            code,
            format!("{operation}: uiautomation get_control_view_walker failed: {error}"),
        )
    })?;
    let element = locate_root_element(&automation, &walker, root, code)?;
    Ok(AutomationRoot {
        automation,
        walker,
        element,
    })
}

fn create_automation(
    code: BackendErrorCode,
    operation: &str,
) -> Result<UIAutomation, BackendError> {
    AUTOMATION.with(|slot| {
        let mut slot = slot.try_borrow_mut().map_err(|error| {
            backend_error(
                code,
                format!("{operation}: UIAutomation cache was already borrowed: {error}"),
            )
        })?;
        if let Some(automation) = slot.as_ref() {
            return Ok(automation.clone());
        }
        let automation = UIAutomation::new().map_err(|error| {
            backend_error(
                code,
                format!("{operation}: UIAutomation::new failed: {error}"),
            )
        })?;
        *slot = Some(automation.clone());
        Ok(automation)
    })
}

fn locate_root_element(
    automation: &UIAutomation,
    walker: &UITreeWalker,
    root: &RootInfo,
    code: BackendErrorCode,
) -> Result<UIElement, BackendError> {
    if let Ok(raw_handle) = isize::try_from(root.window_id) {
        let handle = Handle::from(raw_handle);
        if !handle.is_invalid()
            && let Ok(element) = automation.element_from_handle(handle)
            && element.get_process_id().ok() == Some(root.pid)
        {
            return Ok(element);
        }
    }

    let desktop = automation.get_root_element().map_err(|error| {
        backend_error(
            code,
            format!(
                "uiautomation could not reopen root {}: {error}",
                root.ref_id
            ),
        )
    })?;
    walker
        .get_children(&desktop)
        .unwrap_or_default()
        .into_iter()
        .filter(|element| element.get_process_id().ok() == Some(root.pid))
        .max_by(|left, right| {
            root_match_score(left, root).total_cmp(&root_match_score(right, root))
        })
        .ok_or_else(|| {
            backend_error(
                BackendErrorCode::RootNotFound,
                format!(
                    "no top-level UIAutomation element matched root {}",
                    root.ref_id
                ),
            )
        })
}

fn root_match_score(element: &UIElement, root: &RootInfo) -> f64 {
    let title = element.get_name().unwrap_or_default();
    let frame = element_frame(element);
    let title_score = if !root.title.is_empty() && title == root.title {
        1_000_000.0
    } else if !root.title.is_empty()
        && title
            .to_ascii_lowercase()
            .contains(&root.title.to_ascii_lowercase())
    {
        100_000.0
    } else {
        0.0
    };
    let frame_distance = (frame.x - root.frame.x).abs()
        + (frame.y - root.frame.y).abs()
        + (frame.w - root.frame.w).abs()
        + (frame.h - root.frame.h).abs();
    title_score - frame_distance
}

struct ElementNode {
    element: UIElement,
    node: UiNode,
    children: Vec<ElementNode>,
}

impl ElementNode {
    fn into_ui_node(self) -> UiNode {
        let mut node = self.node;
        node.children = self.children.into_iter().map(Self::into_ui_node).collect();
        node
    }

    fn element_at_path(&self, path: &[usize]) -> Option<&UIElement> {
        let mut current = self;
        for &index in path {
            current = current.children.get(index)?;
        }
        Some(&current.element)
    }
}

struct WalkContext<'a> {
    count: usize,
    visited: HashSet<Vec<i32>>,
    root_frame: Frame,
    walker: &'a UITreeWalker,
}

fn observe_tree(opened: &AutomationRoot, root: &RootInfo) -> Result<UiNode, BackendError> {
    let mut context = WalkContext {
        count: 0,
        visited: HashSet::new(),
        root_frame: root.frame,
        walker: &opened.walker,
    };
    let mut tree = walk_element(&opened.element, 0, &mut context)
        .ok_or_else(|| {
            backend_error(
                BackendErrorCode::ObservationFailed,
                format!("the uiautomation tree for {} was empty", root.title),
            )
        })?
        .into_ui_node();
    if tree.title.is_empty() {
        tree.title.clone_from(&root.title);
    }
    if !tree.frame.has_area() {
        tree.frame = root.frame;
    }
    Ok(tree)
}

fn walk_element(
    element: &UIElement,
    depth: usize,
    context: &mut WalkContext<'_>,
) -> Option<ElementNode> {
    if context.count >= MAX_NODES {
        return None;
    }
    if let Ok(runtime_id) = element.get_runtime_id()
        && !runtime_id.is_empty()
        && !context.visited.insert(runtime_id)
    {
        return None;
    }
    context.count += 1;

    let title = element.get_name().unwrap_or_default();
    let value = element_value(element);
    let description = [element.get_help_text(), element.get_item_status()]
        .into_iter()
        .filter_map(Result::ok)
        .find(|description| !description.is_empty() && description != &title)
        .unwrap_or_default();
    let mut tree = ElementNode {
        element: element.clone(),
        node: UiNode {
            ref_id: String::new(),
            role: element_role(element),
            title,
            value,
            description,
            frame: element_frame(element),
            actions: element_actions(element),
            enabled: element.is_enabled().unwrap_or(true),
            focused: element.has_keyboard_focus().unwrap_or(false),
            children: Vec::new(),
        },
        children: Vec::new(),
    };

    if depth < MAX_DEPTH && context.count < MAX_NODES {
        let remaining = MAX_NODES - context.count;
        for child in raw_children(
            context.walker,
            element,
            remaining.min(MAX_CHILDREN_PER_NODE),
        ) {
            let Some(child) = walk_element(&child, depth + 1, context) else {
                continue;
            };
            if !is_invisible_leaf(&child, context.root_frame) {
                tree.children.push(child);
            }
            if context.count >= MAX_NODES {
                break;
            }
        }
    }
    Some(tree)
}

fn raw_children(walker: &UITreeWalker, element: &UIElement, maximum: usize) -> Vec<UIElement> {
    if maximum == 0 {
        return Vec::new();
    }
    let Ok(mut child) = walker.get_first_child(element) else {
        return Vec::new();
    };
    let mut children = Vec::with_capacity(maximum.min(32));
    loop {
        children.push(child.clone());
        if children.len() >= maximum {
            break;
        }
        let Ok(sibling) = walker.get_next_sibling(&child) else {
            break;
        };
        child = sibling;
    }
    children
}

fn is_invisible_leaf(tree: &ElementNode, root_frame: Frame) -> bool {
    tree.children.is_empty()
        && !tree.node.is_interactive()
        && tree.node.title.is_empty()
        && tree.node.value.is_empty()
        && (!tree.node.frame.has_area()
            || (root_frame.has_area() && !tree.node.frame.intersects(root_frame)))
}

fn element_role(element: &UIElement) -> String {
    element
        .get_control_type()
        .map(|control_type| role_for_control_type(control_type as i32).to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn element_frame(element: &UIElement) -> Frame {
    element
        .get_bounding_rectangle()
        .map(|rect| {
            frame_from_uia_rect(
                rect.get_left(),
                rect.get_top(),
                rect.get_right(),
                rect.get_bottom(),
            )
        })
        .unwrap_or_default()
}

fn element_window_id(element: &UIElement) -> Option<u32> {
    let handle = element.get_native_window_handle().ok()?;
    if handle.is_invalid() {
        return None;
    }
    let raw: isize = handle.into();
    u32::try_from(raw).ok().filter(|window_id| *window_id != 0)
}

fn element_value(element: &UIElement) -> String {
    if let Ok(pattern) = element.get_pattern::<UIValuePattern>()
        && let Ok(value) = pattern.get_value()
    {
        return value;
    }
    if let Ok(pattern) = element.get_pattern::<UIRangeValuePattern>()
        && let Ok(value) = pattern.get_value()
    {
        return value.to_string();
    }
    if let Ok(pattern) = element.get_pattern::<UITogglePattern>()
        && let Ok(value) = pattern.get_toggle_state()
    {
        return match value {
            ToggleState::On => "true".into(),
            ToggleState::Off => "false".into(),
            ToggleState::Indeterminate => "mixed".into(),
        };
    }
    if let Ok(pattern) = element.get_pattern::<UIExpandCollapsePattern>()
        && let Ok(value) = pattern.get_state()
    {
        return match value {
            ExpandCollapseState::Expanded => "expanded".into(),
            ExpandCollapseState::Collapsed => "collapsed".into(),
            ExpandCollapseState::PartiallyExpanded => "partially_expanded".into(),
            ExpandCollapseState::LeafNode => String::new(),
        };
    }
    if let Ok(pattern) = element.get_pattern::<UISelectionItemPattern>()
        && let Ok(value) = pattern.is_selected()
    {
        return value.to_string();
    }
    if let Ok(pattern) = element.get_pattern::<UILegacyIAccessiblePattern>()
        && let Ok(value) = pattern.get_value()
    {
        return value;
    }
    String::new()
}

fn element_actions(element: &UIElement) -> Vec<String> {
    let value_writable = element
        .get_pattern::<UIValuePattern>()
        .ok()
        .is_some_and(|pattern| pattern.is_readonly().map(|value| !value).unwrap_or(true));
    let range_value_writable = element
        .get_pattern::<UIRangeValuePattern>()
        .ok()
        .is_some_and(|pattern| pattern.is_readonly().map(|value| !value).unwrap_or(true));
    actions_for_patterns(PatternSupport {
        invoke: element.get_pattern::<UIInvokePattern>().is_ok(),
        value_writable,
        range_value_writable,
        toggle: element.get_pattern::<UITogglePattern>().is_ok(),
        expand_collapse: element.get_pattern::<UIExpandCollapsePattern>().is_ok(),
        scroll: element.get_pattern::<UIScrollPattern>().is_ok(),
        scroll_item: element.get_pattern::<UIScrollItemPattern>().is_ok(),
        selection_item: element.get_pattern::<UISelectionItemPattern>().is_ok(),
        legacy_default: element
            .get_pattern::<UILegacyIAccessiblePattern>()
            .ok()
            .and_then(|pattern| pattern.get_default_action().ok())
            .is_some_and(|action| !action.trim().is_empty()),
    })
}

struct Target {
    _automation: UIAutomation,
    walker: UITreeWalker,
    element: UIElement,
}

impl Target {
    fn press(&self) -> Result<&'static str, UiaFailure> {
        let mut failures = Vec::new();
        if let Ok(pattern) = self.element.get_pattern::<UIInvokePattern>() {
            match pattern.invoke() {
                Ok(()) => return Ok("uiautomation InvokePattern completed"),
                Err(error) => failures.push(format!("InvokePattern failed: {error}")),
            }
        }
        if let Ok(pattern) = self.element.get_pattern::<UITogglePattern>() {
            match pattern.toggle() {
                Ok(()) => return Ok("uiautomation TogglePattern completed"),
                Err(error) => failures.push(format!("TogglePattern failed: {error}")),
            }
        }
        if let Ok(pattern) = self.element.get_pattern::<UIExpandCollapsePattern>() {
            let result = match pattern.get_state() {
                Ok(ExpandCollapseState::Expanded | ExpandCollapseState::PartiallyExpanded) => {
                    pattern
                        .collapse()
                        .map(|()| "uiautomation ExpandCollapsePattern collapsed")
                }
                _ => pattern
                    .expand()
                    .map(|()| "uiautomation ExpandCollapsePattern expanded"),
            };
            match result {
                Ok(message) => return Ok(message),
                Err(error) => failures.push(format!("ExpandCollapsePattern failed: {error}")),
            }
        }
        if let Ok(pattern) = self.element.get_pattern::<UISelectionItemPattern>() {
            match pattern.select() {
                Ok(()) => return Ok("uiautomation SelectionItemPattern selected the target"),
                Err(error) => failures.push(format!("SelectionItemPattern failed: {error}")),
            }
        }
        if let Ok(pattern) = self.element.get_pattern::<UILegacyIAccessiblePattern>()
            && pattern
                .get_default_action()
                .is_ok_and(|action| !action.trim().is_empty())
        {
            match pattern.do_default_action() {
                Ok(()) => return Ok("uiautomation legacy default action completed"),
                Err(error) => failures.push(format!("legacy default action failed: {error}")),
            }
        }
        Err(UiaFailure::from_attempts("press", failures))
    }

    fn set_text(&self, text: &str) -> Result<&'static str, UiaFailure> {
        let mut failures = Vec::new();
        if let Ok(pattern) = self.element.get_pattern::<UIValuePattern>() {
            match pattern.set_value(text) {
                Ok(()) => return Ok("uiautomation ValuePattern value was set"),
                Err(error) => failures.push(format!("ValuePattern failed: {error}")),
            }
        }
        if let Ok(pattern) = self.element.get_pattern::<UIRangeValuePattern>() {
            match text.parse::<f64>() {
                Ok(value) => match pattern.set_value(value) {
                    Ok(()) => return Ok("uiautomation RangeValuePattern value was set"),
                    Err(error) => failures.push(format!("RangeValuePattern failed: {error}")),
                },
                Err(error) => {
                    failures.push(format!("RangeValuePattern requires a number: {error}"));
                }
            }
        }
        if let Ok(pattern) = self.element.get_pattern::<UILegacyIAccessiblePattern>() {
            match pattern.set_value(text) {
                Ok(()) => return Ok("uiautomation legacy value was set"),
                Err(error) => failures.push(format!("legacy value failed: {error}")),
            }
        }
        Err(UiaFailure::from_attempts("set_text", failures))
    }

    fn focus(&self) -> Result<(), UiaFailure> {
        self.element
            .set_focus()
            .map_err(|error| UiaFailure(format!("uiautomation set_focus failed: {error}")))
    }

    fn scroll(&self, x: f64, y: f64) -> Result<&'static str, UiaFailure> {
        let horizontal = scroll_amount(x, false);
        let vertical = scroll_amount(y, true);
        let mut failures = Vec::new();
        let mut current = self.element.clone();
        for _ in 0..=MAX_DEPTH {
            if let Ok(pattern) = current.get_pattern::<UIScrollPattern>() {
                match pattern.scroll(horizontal, vertical) {
                    Ok(()) => return Ok("uiautomation ScrollPattern completed"),
                    Err(error) => failures.push(format!("ScrollPattern failed: {error}")),
                }
            }
            let Ok(parent) = self.walker.get_parent(&current) else {
                break;
            };
            current = parent;
        }
        if let Ok(pattern) = self.element.get_pattern::<UIScrollItemPattern>() {
            match pattern.scroll_into_view() {
                Ok(()) => {
                    return Ok("uiautomation ScrollItemPattern scrolled the target into view");
                }
                Err(error) => failures.push(format!("ScrollItemPattern failed: {error}")),
            }
        }
        Err(UiaFailure::from_attempts("scroll", failures))
    }

    fn frame(&self) -> Frame {
        element_frame(&self.element)
    }
}

#[derive(Debug)]
struct UiaFailure(String);

impl UiaFailure {
    fn from_attempts(operation: &str, failures: Vec<String>) -> Self {
        if failures.is_empty() {
            Self(format!(
                "uiautomation target exposes no pattern for {operation}"
            ))
        } else {
            Self(failures.join("; "))
        }
    }
}

impl fmt::Display for UiaFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn target(root: &RootInfo, request: &ActionRequest) -> Result<Target, BackendError> {
    let path = request.target_path.as_deref().ok_or_else(|| {
        BackendError::new(
            BackendErrorCode::InvalidAction,
            "this action requires an element ref",
        )
    })?;
    locate_target(
        root,
        path,
        request.target_role.as_deref(),
        request.target_title.as_deref(),
    )
}

fn locate_target(
    root: &RootInfo,
    path: &[usize],
    expected_role: Option<&str>,
    expected_title: Option<&str>,
) -> Result<Target, BackendError> {
    let opened = open_root(
        root,
        BackendErrorCode::OperationFailed,
        "opening the UIAutomation root for an action",
    )?;
    let mut context = WalkContext {
        count: 0,
        visited: HashSet::new(),
        root_frame: root.frame,
        walker: &opened.walker,
    };
    let tree = walk_element(&opened.element, 0, &mut context).ok_or_else(|| {
        backend_error(
            BackendErrorCode::OperationFailed,
            "the target's UIAutomation tree disappeared; call observe_ui again",
        )
    })?;
    let element = tree.element_at_path(path).cloned().ok_or_else(|| {
        backend_error(
            BackendErrorCode::OperationFailed,
            "the target's UIAutomation path moved; call observe_ui again",
        )
    })?;

    if let Some(expected_role) = expected_role {
        let actual = element_role(&element);
        if canonical_role(&actual) != canonical_role(expected_role) {
            return Err(backend_error(
                BackendErrorCode::OperationFailed,
                format!(
                    "target role changed from {} to {}; call observe_ui again",
                    canonical_role(expected_role),
                    canonical_role(&actual)
                ),
            ));
        }
    }
    if let Some(expected_title) = expected_title.filter(|title| !title.is_empty()) {
        let actual = element.get_name().unwrap_or_default();
        if !actual.is_empty() && actual != expected_title {
            return Err(backend_error(
                BackendErrorCode::OperationFailed,
                "target title changed; call observe_ui again",
            ));
        }
    }
    Ok(Target {
        _automation: opened.automation,
        walker: opened.walker,
        element,
    })
}

fn scroll_target(root: &RootInfo, request: &ActionRequest) -> Result<Target, BackendError> {
    if request.target_path.is_some() {
        return target(root, request);
    }
    let point = if let (Some(x), Some(y)) = (request.x, request.y) {
        Some(point_for_uia(x, y)?)
    } else {
        request
            .target_frame
            .filter(|frame| frame.has_area())
            .map(Frame::center)
            .map(|(x, y)| point_for_uia(x, y))
            .transpose()?
    };
    let opened = open_root(
        root,
        BackendErrorCode::OperationFailed,
        "opening the UIAutomation root for scroll",
    )?;
    let element = if let Some(point) = point {
        opened
            .automation
            .element_from_point(point)
            .map_err(|error| {
                backend_error(
                    BackendErrorCode::OperationFailed,
                    format!("uiautomation element_from_point failed for scroll: {error}"),
                )
            })?
    } else {
        opened.element
    };
    Ok(Target {
        _automation: opened.automation,
        walker: opened.walker,
        element,
    })
}

fn action_point(root: &RootInfo, request: &ActionRequest) -> Result<(f64, f64), BackendError> {
    if let (Some(x), Some(y)) = (request.x, request.y) {
        return Ok((x, y));
    }
    if request.target_path.is_some() {
        let live_frame = target(root, request)?.frame();
        if live_frame.has_area() {
            return Ok(live_frame.center());
        }
    }
    if let Some(frame) = request.target_frame.filter(|frame| frame.has_area()) {
        return Ok(frame.center());
    }
    Err(BackendError::new(
        BackendErrorCode::InvalidAction,
        "action requires x/y coordinates or an element with a non-empty frame",
    ))
}

fn point_for_uia(x: f64, y: f64) -> Result<Point, BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(backend_error(
            BackendErrorCode::InvalidAction,
            "scroll coordinates must be finite",
        ));
    }
    let x = x.round();
    let y = y.round();
    if x < f64::from(i32::MIN)
        || x > f64::from(i32::MAX)
        || y < f64::from(i32::MIN)
        || y > f64::from(i32::MAX)
    {
        return Err(backend_error(
            BackendErrorCode::InvalidAction,
            "scroll coordinates exceed the Windows range",
        ));
    }
    Ok(Point::new(x as i32, y as i32))
}

fn validate_scroll_deltas(x: f64, y: f64) -> Result<(), BackendError> {
    if x.is_finite() && y.is_finite() {
        Ok(())
    } else {
        Err(backend_error(
            BackendErrorCode::InvalidAction,
            "scroll deltas must be finite",
        ))
    }
}

fn scroll_amount(value: f64, invert: bool) -> ScrollAmount {
    let value = if invert { -value } else { value };
    if value == 0.0 {
        ScrollAmount::NoAmount
    } else if value >= 120.0 {
        ScrollAmount::LargeIncrement
    } else if value > 0.0 {
        ScrollAmount::SmallIncrement
    } else if value <= -120.0 {
        ScrollAmount::LargeDecrement
    } else {
        ScrollAmount::SmallDecrement
    }
}

fn process_executable_name(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let _handle = OwnedProcessHandle(handle);
    let mut buffer = vec![0_u16; PROCESS_PATH_CAPACITY];
    let mut length = u32::try_from(buffer.len()).ok()?;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    let length = usize::try_from(length).ok()?.min(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..length]);
    nonempty(
        path.rsplit(['\\', '/'])
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

struct OwnedProcessHandle(HANDLE);

impl Drop for OwnedProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn app_display_name(executable_name: &str, class_name: &str) -> String {
    if !executable_name.is_empty() {
        if executable_name
            .get(executable_name.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
        {
            return executable_name[..executable_name.len() - 4].to_string();
        }
        return executable_name.to_string();
    }
    class_name.to_string()
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn backend_error(code: BackendErrorCode, message: impl Into<String>) -> BackendError {
    BackendError::new(code, message)
}

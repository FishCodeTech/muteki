use std::{cell::RefCell, sync::Arc};

use super::{
    inline::{Inline, InlineState},
    math_layout::{self, FormulaSelection},
    render::RecentCache,
    selection::RunOrigin,
    state::{MarkdownState, PendingContextTarget},
};
use crate::{
    icon::IconName,
    scroll::ScrollableElement as _,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::copy::{action_button, copy_button},
};
use gpui::{
    AnyElement, App, Entity, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::h_flex;
use latex_rust::{MathBox, MathFont, MathStyle, layout, parse};

type FormulaCache = RecentCache<(String, bool), Option<Arc<MathBox>>>;

thread_local! {
    static FONT: Option<MathFont> = MathFont::stix_two_math().ok();
    static FORMULAS: RefCell<FormulaCache> = RefCell::new(RecentCache::new(128));
}

pub(super) fn is_math(lang: Option<&str>) -> bool {
    lang.is_some_and(|lang| {
        ["latex", "tex", "math"]
            .iter()
            .any(|name| lang.eq_ignore_ascii_case(name))
    })
}

fn formula(source: &str, display: bool) -> Option<Arc<MathBox>> {
    FORMULAS.with(|cache| {
        cache
            .borrow_mut()
            .get_or_insert_with((source.into(), display), || {
                FONT.with(|font| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        layout(
                            &parse(source).ok()?,
                            font.as_ref()?,
                            if display {
                                MathStyle::Display
                            } else {
                                MathStyle::Text
                            },
                        )
                        .ok()
                        .map(Arc::new)
                    }))
                    .ok()
                    .flatten()
                })
            })
    })
}

/// Records the formula under a right-click for the view's context menu.
fn on_right_click(
    view: &Entity<MarkdownState>,
    target: PendingContextTarget,
) -> impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static {
    let view = view.clone();
    move |_, _, cx| {
        view.update(cx, |state, cx| {
            state.set_pending_context(Some(target.clone()), cx)
        })
    }
}

pub(super) fn block(
    source: &str,
    selection: &FormulaSelection,
    code: AnyElement,
    path: &str,
    view: &Entity<MarkdownState>,
    window: &Window,
    cx: &mut App,
) -> AnyElement {
    let tree = formula(source, true);
    selection.set_active(false);
    let show_source = view.read(cx).math_sources.contains(path);
    let compact = crate::window_seam::window_is_compact(window, cx);
    let latex = source.strip_suffix('\n').unwrap_or(source).to_string();
    let group = SharedString::from(format!("math-block-{path}"));

    let toggle = tree.is_some().then(|| {
        let (view, path) = (view.clone(), path.to_string());
        let (icon, label) = if show_source {
            (IconName::Sigma, crate::tr!("markdown.math_show_rendered"))
        } else {
            (IconName::Code, crate::tr!("markdown.math_show_source"))
        };
        #[cfg(test)]
        let selector = format!("markdown-math-toggle-{path}");
        let button = action_button(format!("math-toggle-{path}"), icon, label.into_owned(), cx)
            .touch(compact)
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                view.update(cx, |state, cx| {
                    state.set_math_source(&path, !show_source, cx)
                });
            });
        #[cfg(test)]
        let button = div().debug_selector(move || selector).child(button);
        button
    });
    let copy = {
        let (view, path, latex) = (view.clone(), path.to_string(), latex.clone());
        copy_button(
            &format!("math-{path}"),
            view.read(cx).copied.is(&path),
            compact,
            move |_, _, cx| {
                cx.stop_propagation();
                view.update(cx, |state, cx| {
                    state.copy_code(path.clone(), latex.clone(), cx)
                });
            },
            cx,
        )
    };
    // Revealed on hover like the message actions, and pinned where there is
    // no hover or while the source shows. Faded rather than hidden: over its
    // own mouse-blocking hitbox the block no longer counts as hovered, so its
    // own hover must keep it shown. Pinned beside a rendered formula on a
    // narrow screen, so it never covers the formula's end.
    let beside = compact && tree.is_some() && !show_source;
    let actions = h_flex()
        .when(!beside, |actions| actions.absolute().top_1().right_1())
        .when(beside, |actions| actions.flex_none().mt_1())
        .items_center()
        .block_mouse_except_scroll()
        .rounded(crate::material::radius_card(cx))
        .bg(cx.theme().tokens.colors.muted)
        .when(tree.is_none(), |actions| {
            actions.pl_2().child(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("markdown.math_error").into_owned()),
            )
        })
        .children(toggle)
        .child(copy)
        .when(tree.is_some() && !show_source && !compact, |actions| {
            actions
                .opacity(0.)
                .group_hover(group.clone(), |style| style.opacity(1.))
                .hover(|style| style.opacity(1.))
        });

    // The actions sit on the box that shows: the formula, or its source.
    let shown = match &tree {
        Some(tree) if !show_source => div()
            .p_3()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .child(math_layout::render(
                        tree,
                        selection,
                        Some(RunOrigin::new(super::selection::render_path(path))),
                        view,
                        cx,
                    ))
                    .overflow_x_scroll_area(),
            )
            .into_any_element(),
        _ => div().flex_1().min_w_0().child(code).into_any_element(),
    };
    let body = h_flex()
        .relative()
        .w_full()
        .items_start()
        .child(shown)
        .child(actions);
    let block = div()
        .group(group)
        .w_full()
        .on_mouse_down(
            MouseButton::Right,
            on_right_click(
                view,
                PendingContextTarget::Math {
                    path: path.to_string(),
                    latex,
                    show_source: tree.is_some().then_some(show_source),
                },
            ),
        )
        .child(body);
    #[cfg(test)]
    let block = {
        let path = path.to_string();
        block.debug_selector(move || format!("markdown-math-{path}"))
    };
    block.into_any_element()
}

pub(super) fn inline(
    math: &super::math_parse::MathSpan,
    raw_state: &Arc<std::sync::Mutex<InlineState>>,
    path: &str,
    origin: RunOrigin,
    view: &Entity<MarkdownState>,
    _: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let show_source = view.read(cx).math_sources.contains(path);
    math.selection.set_active(false);
    let tree = formula(&math.source, false);
    let body = if !show_source && let Some(tree) = &tree {
        math_layout::render(tree, &math.selection, Some(origin), view, cx)
    } else {
        div()
            .font_family(cx.theme().mono_font_family.clone())
            .child(
                Inline::new(
                    "math-source",
                    view.clone(),
                    raw_state.clone(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )
                .origin(Some(origin)),
            )
            .into_any_element()
    };
    h_flex()
        .on_mouse_down(
            MouseButton::Right,
            on_right_click(
                view,
                PendingContextTarget::Math {
                    path: path.to_string(),
                    latex: math.source.clone(),
                    show_source: tree.is_some().then_some(show_source),
                },
            ),
        )
        .child(body)
        .into_any_element()
}

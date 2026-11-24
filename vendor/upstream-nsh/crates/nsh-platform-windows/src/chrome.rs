//! The window's chrome (`nsh/desktop`): its title, the hidden title bar,
//! full screen, the menu bar and context menus.
//!
//! - **Hidden title bar** ([`TitleBarStyle::Hidden`]): `WM_NCCALCSIZE`
//!   gives the client area the window's whole height (the side and bottom
//!   resize borders stay non-client, invisible as on any Windows 10 / 11
//!   window), and `WM_NCHITTEST` answers for the top: its resize border,
//!   the caption buttons (`HTMINBUTTON`, `HTMAXBUTTON`, `HTCLOSE`, so that
//!   Windows 11 shows its snap layouts over the maximize button) and the
//!   views marked `windowDrag` (`HTCAPTION`: the system moves the window,
//!   maximizes it on a double click and opens its menu on a right click).
//!   The caption buttons are drawn over the app ([`draw`]), in Windows 11's
//!   style, following the app's appearance; non-client clicks on them are
//!   handled here, never by `DefWindowProc`, which would draw Windows 95's.
//! - **Drag regions** work with the system title bar too: a view marked
//!   `windowDrag` answers `HTCAPTION` anywhere.
//! - **Menus** are Win32 menus: the menu bar (`SetMenu`, not shown while
//!   the title bar is hidden) and `TrackPopupMenuEx` for context menus,
//!   both themed dark with the app (uxtheme's `SetPreferredAppMode` for the
//!   menus, the `WM_UAHDRAWMENU*` messages for the bar, which has no dark
//!   theme of its own). Their shortcuts are matched here before the keys
//!   reach the app; the editing roles act on the focused text field by
//!   sending it their keys.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use nsh_core::{
    Color, EventKind, EventPayload, PayloadValue, ROOT_ID,
    desktop::{
        DesktopCommand, Key, MenuAction, MenuItem, MenuRole, Shortcut, TitleBarStyle, WindowFrame,
        WindowLevel,
    },
    geometry::{Edges, Points},
};
use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Direct2D::ID2D1DeviceContext,
            Gdi::{
                CreateSolidBrush, DT_CENTER, DT_HIDEPREFIX, DT_SINGLELINE, DT_VCENTER,
                DeleteObject, DrawTextW, FillRect, GetWindowDC, HDC, ReleaseDC, SetBkMode,
                SetTextColor, TRANSPARENT,
            },
        },
        System::LibraryLoader::{GetProcAddress, LoadLibraryW},
        UI::{
            Controls::{
                DRAWITEMSTRUCT, ODS_DISABLED, ODS_GRAYED, ODS_HOTLIGHT, ODS_INACTIVE, ODS_NOACCEL,
                ODS_SELECTED,
            },
            HiDpi::GetSystemMetricsForDpi,
            Input::KeyboardAndMouse::{
                TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_BACK,
                VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LEFT,
                VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7,
                VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR, VK_RETURN,
                VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{PCSTR, PCWSTR, w},
};
use windows_numerics::Vector2;

use crate::{
    input::{KeyEvent, Modifiers},
    render::{color_f, rect_f},
    surface::Surface,
};

/// The hidden title bar's height when the app gives none, in points
/// (Windows 11's caption).
const DEFAULT_CAPTION_HEIGHT: f32 = 32.0;

/// The hidden title bar's height, in points: the app's, else Windows'.
fn caption_height(surface: &Surface) -> f32 {
    surface
        .chrome
        .height
        .get()
        .unwrap_or(DEFAULT_CAPTION_HEIGHT)
}
/// A caption button's width, in points.
const BUTTON_WIDTH: f32 = 46.0;

/// The undocumented messages that draw the menu bar (uxtheme's
/// "UAH" menus): the whole bar, then each item.
const WM_UAHDRAWMENU: u32 = 0x0091;
const WM_UAHDRAWMENUITEM: u32 = 0x0092;

/// What `WM_UAHDRAWMENU` points to.
#[repr(C)]
struct UahMenu {
    hmenu: HMENU,
    hdc: HDC,
    flags: u32,
}

/// What `WM_UAHDRAWMENUITEM` points to.
#[repr(C)]
struct UahDrawMenuItem {
    dis: DRAWITEMSTRUCT,
    um: UahMenu,
    /// The item's position in the bar (then metrics nobody reads here).
    position: i32,
}

/// An item of a Win32 menu: what choosing it does.
#[derive(Clone, Copy)]
struct Entry {
    /// The app's id (0: none).
    id: u32,
    role: Option<MenuRole>,
}

/// A context menu: its items and where it shows.
struct Popup {
    items: Arc<[MenuItem]>,
    at: Option<(f32, f32)>,
    /// What the items of a menu of the platform's own do, by id - 1 (the
    /// app's menus report their ids instead).
    actions: Vec<Action>,
}

/// What an item of a menu of the platform's own does.
pub(crate) type Action = Rc<dyn Fn(&Rc<Surface>)>;

/// A surface's chrome.
#[derive(Default)]
pub(crate) struct Chrome {
    style: Cell<TitleBarStyle>,
    /// The app's title bar's height (`Window.setTitleBar`'s).
    height: Cell<Option<f32>>,
    /// The caption button under the mouse (`HT*`, 0 for none).
    hover: Cell<u32>,
    /// The caption button pressed.
    pressed: Cell<u32>,
    /// The mouse is tracked for `WM_NCMOUSELEAVE`.
    tracking: Cell<bool>,
    /// The window is inactive (its caption buttons dim).
    inactive: Cell<bool>,
    /// The application menu.
    menu: RefCell<Option<Arc<[MenuItem]>>>,
    /// The menu bar shown (`HMENU`, 0 for none).
    bar: Cell<isize>,
    /// What the menu bar's items do, by command id - 1.
    entries: RefCell<Vec<Entry>>,
    /// The context menu to show once the batch is applied.
    popup: RefCell<Option<Popup>>,
    /// The app's context menu tracking now, and the item a test chose
    /// from it (`nsh test --target windows`), carried out once it closes.
    #[cfg(feature = "dev")]
    tracked_popup: RefCell<Option<Arc<[MenuItem]>>>,
    #[cfg(feature = "dev")]
    test_choice: Cell<Option<(u32, Option<MenuRole>)>>,
}

/// Chooses the item of the app's open context menu at the end of `path`
/// (the labels of the submenus on the way, then its own), as a click on it
/// does: the menu ends, then the item is carried out (`nsh test --target
/// windows`).
#[cfg(feature = "dev")]
pub(crate) fn choose_open_menu(surface: &Surface, path: &[String]) -> Result<(), String> {
    let items = surface
        .chrome
        .tracked_popup
        .borrow()
        .clone()
        .ok_or("no context menu is open")?;
    let mut level: &[MenuItem] = &items;
    for (n, label) in path.iter().enumerate() {
        let last = n + 1 == path.len();
        let item = level
            .iter()
            .find(|item| match item {
                MenuItem::Action(action) => last && action.label == *label,
                MenuItem::Submenu { label: l, .. } => !last && l == label,
                MenuItem::Separator => false,
            })
            .ok_or_else(|| format!("the menu has no item {label:?}"))?;
        match item {
            MenuItem::Action(action) => {
                surface
                    .chrome
                    .test_choice
                    .set(Some((action.id, action.role)));
            }
            MenuItem::Submenu { items, .. } => level = items,
            MenuItem::Separator => {}
        }
    }
    // SAFETY: ends the menu this thread tracks.
    let _ = unsafe { EndMenu() };
    Ok(())
}

/// Closes the app's open context menu with nothing chosen.
#[cfg(feature = "dev")]
pub(crate) fn close_open_menu(surface: &Surface) -> Result<(), String> {
    if surface.chrome.tracked_popup.borrow().is_none() {
        return Err("no context menu is open".into());
    }
    // SAFETY: ends the menu this thread tracks.
    let _ = unsafe { EndMenu() };
    Ok(())
}

fn top_level(hwnd: HWND) -> bool {
    // SAFETY: a plain query of a window of this thread.
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    !hwnd.is_invalid() && style & WS_CHILD.0 == 0
}

fn zoomed(hwnd: HWND) -> bool {
    // SAFETY: a plain query.
    unsafe { IsZoomed(hwnd) }.as_bool()
}

/// The title bar is hidden now (not in full screen, which has none).
pub(crate) fn hidden(surface: &Surface) -> bool {
    let c = &surface.chrome;
    c.style.get() == TitleBarStyle::Hidden
        && !crate::window::is_full_screen(surface.hwnd())
        && top_level(surface.hwnd())
}

/// The resize border's thickness in pixels (also what a maximized window
/// hangs off its monitor by).
fn frame_thickness(hwnd: HWND) -> i32 {
    let dpi = crate::window::dpi(hwnd);
    // SAFETY: plain metric queries.
    unsafe {
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

/// Tells the app the title bar's area.
fn update_area(surface: &Surface) {
    let mut env = surface.environment.get();
    env.title_bar = if hidden(surface) {
        Edges {
            top: Points(caption_height(surface)),
            right: Points(3.0 * BUTTON_WIDTH),
            bottom: Points(0.0),
            left: Points(0.0),
        }
    } else {
        Edges::default()
    };
    surface.set_environment(env);
}

/// The frame changed: the window recomputes its client area.
fn frame_changed(hwnd: HWND) {
    // SAFETY: a plain call on a live window.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

// --- Commands ---------------------------------------------------------------------------

/// A desktop command (on the root).
pub(crate) fn command(surface: &Rc<Surface>, command: &DesktopCommand) {
    let hwnd = surface.hwnd();
    if hwnd.is_invalid() {
        return;
    }
    // The embedding app owns its window: only context menus work there.
    if !top_level(hwnd) && !matches!(command, DesktopCommand::PopupMenu { .. }) {
        return;
    }
    let c = &surface.chrome;
    match command {
        DesktopCommand::SetTitle(title) => {
            // SAFETY: a plain call with a NUL-terminated string.
            let _ = unsafe { SetWindowTextW(hwnd, &windows::core::HSTRING::from(title.as_str())) };
        }
        DesktopCommand::SetTitleBar { style, height } => {
            let height = height.map(|h| h.0);
            let changed = c.height.replace(height) != height;
            if c.style.replace(*style) != *style || changed {
                show_menu_bar(surface);
                frame_changed(hwnd);
                update_area(surface);
                surface.invalidate();
            }
        }
        DesktopCommand::Minimize => {
            // SAFETY: a plain call.
            let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
        }
        DesktopCommand::ToggleMaximize => toggle_maximize(surface),
        DesktopCommand::ToggleFullScreen => toggle_fullscreen(surface),
        DesktopCommand::SetFrame(frame) => set_frame(surface, hwnd, frame),
        DesktopCommand::SetLevel(level) => set_level(hwnd, *level),
        DesktopCommand::OrderFront => {
            // Restoring an already maximized or snapped window would change
            // its bounds when the caller only asked to focus it.
            // SAFETY: hwnd is the surface's live top-level window.
            unsafe {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
            }
        }
        DesktopCommand::Close => close(hwnd),
        DesktopCommand::SetMenu(menu) => {
            *c.menu.borrow_mut() = menu.clone();
            show_menu_bar(surface);
        }
        // The Win32 menu bar always shows.
        DesktopCommand::SetMenuBar(_) => {}
        DesktopCommand::PopupMenu { items, at } => popup(
            surface,
            Popup {
                items: items.clone(),
                at: at.map(|p| (p.x.0, p.y.0)),
                actions: Vec::new(),
            },
        ),
    }
}

/// Moves and sizes the window: a field left out keeps its value. The
/// points of `Window.setFrame` are pixels at `surface.scale()`; Win32
/// works on the outer frame (non-client area included), as does
/// `GetWindowRect`.
fn set_frame(surface: &Surface, hwnd: HWND, frame: &WindowFrame) {
    let mut rect = RECT::default();
    // SAFETY: a plain query with a local out value.
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let scale = surface.scale();
    let x = frame.x.map_or(rect.left, |v| (v.0 * scale).round() as i32);
    let y = frame.y.map_or(rect.top, |v| (v.0 * scale).round() as i32);
    let width = frame
        .width
        .map_or(rect.right - rect.left, |v| (v.0 * scale).round() as i32);
    let height = frame
        .height
        .map_or(rect.bottom - rect.top, |v| (v.0 * scale).round() as i32);
    // SAFETY: a plain call on a live window; SWP_NOZORDER keeps the level
    // `set_level` may have set.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            None,
            x,
            y,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

/// The window's level: `HWND_TOPMOST` floats, `HWND_NOTOPMOST` is the
/// normal level, `HWND_BOTTOM` is the closest Windows has to the
/// desktop's icons (nothing draws below them).
fn set_level(hwnd: HWND, level: WindowLevel) {
    let order = match level {
        WindowLevel::Normal => HWND_NOTOPMOST,
        WindowLevel::Floating => HWND_TOPMOST,
        WindowLevel::DesktopIcon => HWND_BOTTOM,
    };
    // SAFETY: a plain call on a live window.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            Some(order),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
}

/// Shows `menu` from the message loop: `TrackPopupMenuEx` runs one of its
/// own, which must not run while a batch is applied or an event handled.
fn popup(surface: &Surface, menu: Popup) {
    *surface.chrome.popup.borrow_mut() = Some(menu);
    // SAFETY: posting to a window of this thread.
    let _ = unsafe {
        PostMessageW(
            Some(surface.hwnd()),
            crate::window::WM_NSH_MENU,
            WPARAM(0),
            LPARAM(0),
        )
    };
}

/// An item of a menu of the platform's own ([`popup_actions`]).
pub(crate) enum LocalItem {
    /// A line between groups.
    Separator,
    /// An item.
    Item {
        /// Its role: the label and shortcut shown (the action does the
        /// work).
        role: Option<MenuRole>,
        /// A label of its own (else the role's).
        label: &'static str,
        /// It can be chosen.
        enabled: bool,
        /// What choosing it does.
        action: Action,
    },
}

/// Shows a context menu of the platform's own (a text field's) at the
/// pointer.
pub(crate) fn popup_actions(surface: &Surface, items: Vec<LocalItem>) {
    let mut actions = Vec::new();
    let items: Vec<MenuItem> = items
        .into_iter()
        .map(|item| match item {
            LocalItem::Separator => MenuItem::Separator,
            LocalItem::Item {
                role,
                label,
                enabled,
                action,
            } => {
                actions.push(action);
                MenuItem::Action(MenuAction {
                    id: actions.len() as u32,
                    label: if label.is_empty() {
                        role.map_or("", MenuRole::label).to_owned()
                    } else {
                        label.to_owned()
                    },
                    shortcut: role.and_then(|r| r.shortcut(false)),
                    enabled,
                    checked: None,
                    role: None,
                    when: nsh_core::keymap::When::Always,
                })
            }
        })
        .collect();
    popup(
        surface,
        Popup {
            items: items.into(),
            at: None,
            actions,
        },
    );
}

fn close(hwnd: HWND) {
    // SAFETY: posting to a live window.
    let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
}

fn toggle_maximize(surface: &Surface) {
    let hwnd = surface.hwnd();
    if crate::window::is_full_screen(hwnd) {
        return;
    }
    let show = if zoomed(hwnd) {
        SW_RESTORE
    } else {
        SW_MAXIMIZE
    };
    // SAFETY: a plain call.
    let _ = unsafe { ShowWindow(hwnd, show) };
}

/// Enters or leaves full screen (`crate::window::toggle_full_screen`, which
/// F11 runs too; it tells [`full_screen_changed`]).
fn toggle_fullscreen(surface: &Surface) {
    crate::window::toggle_full_screen(surface.hwnd());
}

/// The window entered or left full screen: the menu bar and the title
/// bar's area follow (neither shows in full screen).
pub(crate) fn full_screen_changed(surface: &Surface) {
    show_menu_bar(surface);
    update_area(surface);
    surface.invalidate();
}

// --- The hidden title bar ---------------------------------------------------------------

/// `WM_NCCALCSIZE`: with the title bar hidden, the client area reaches the
/// window's top.
pub(crate) fn nc_calc_size(
    surface: &Surface,
    hwnd: HWND,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if wparam.0 == 0 || !hidden(surface) {
        return None;
    }
    // SAFETY: with `wparam` TRUE, `lparam` is the window's
    // NCCALCSIZE_PARAMS, valid for the message.
    unsafe {
        let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
        let top = params.rgrc[0].top;
        DefWindowProcW(hwnd, WM_NCCALCSIZE, wparam, lparam);
        // A maximized window hangs off its monitor by its resize border;
        // otherwise Windows 11 draws its border along the window's top edge,
        // over whatever is there: the app starts below it, so that its
        // title bar is all visible (and centered as laid out).
        params.rgrc[0].top = top
            + if zoomed(hwnd) {
                frame_thickness(hwnd)
            } else {
                top_border(hwnd)
            };
    }
    Some(LRESULT(0))
}

/// The visible border the system draws along a window's top edge, in
/// pixels (Windows 11; 0 where there is none).
fn top_border(hwnd: HWND) -> i32 {
    use windows::Win32::Graphics::Dwm::{
        DWMWA_VISIBLE_FRAME_BORDER_THICKNESS, DwmGetWindowAttribute,
    };
    let mut thickness: u32 = 0;
    // SAFETY: a u32 out value, as the attribute is.
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_VISIBLE_FRAME_BORDER_THICKNESS,
            &mut thickness as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .is_ok();
    if ok { thickness.min(8) as i32 } else { 0 }
}

/// The caption button at `(x, y)` (points), as its hit test code.
fn caption_at(surface: &Surface, x: f32, y: f32) -> Option<u32> {
    if !hidden(surface) || !(0.0..caption_height(surface)).contains(&y) {
        return None;
    }
    let from_right = surface.size().0 - x;
    match (from_right / BUTTON_WIDTH).floor() {
        0.0 => Some(HTCLOSE),
        1.0 => Some(HTMAXBUTTON),
        2.0 => Some(HTMINBUTTON),
        _ => None,
    }
}

/// `WM_NCHITTEST`: the hidden title bar's resize border and caption
/// buttons, and the views that drag the window.
pub(crate) fn nc_hit_test(surface: &Surface, hwnd: HWND, lparam: LPARAM) -> Option<LRESULT> {
    // SAFETY: default handling finds the non-client resize borders.
    let default = unsafe { DefWindowProcW(hwnd, WM_NCHITTEST, WPARAM(0), lparam) };
    if default.0 as u32 != HTCLIENT {
        return Some(default);
    }
    let mut p = POINT {
        x: (lparam.0 & 0xFFFF) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    };
    // SAFETY: a plain call with a local value.
    let _ = unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut p) };
    let scale = surface.scale();
    let (x, y) = (p.x as f32 / scale, p.y as f32 / scale);
    if hidden(surface) {
        let border = frame_thickness(hwnd);
        if !zoomed(hwnd) && p.y < border {
            let width = surface.pixel_size().0 as i32;
            let code = if p.x < border * 2 {
                HTTOPLEFT
            } else if p.x >= width - border * 2 {
                HTTOPRIGHT
            } else {
                HTTOP
            };
            return Some(LRESULT(code as isize));
        }
        if let Some(code) = caption_at(surface, x, y) {
            return Some(LRESULT(code as isize));
        }
    }
    let code = if crate::input::window_drag(surface, x, y) {
        HTCAPTION
    } else {
        HTCLIENT
    };
    Some(LRESULT(code as isize))
}

fn is_button(code: u32) -> bool {
    matches!(code, HTMINBUTTON | HTMAXBUTTON | HTCLOSE)
}

fn set_hover(surface: &Surface, code: u32) {
    if surface.chrome.hover.replace(code) != code {
        surface.invalidate();
    }
}

/// Non-client mouse messages over the caption buttons.
pub(crate) fn nc_mouse(surface: &Surface, hwnd: HWND, msg: u32, wparam: WPARAM) -> Option<LRESULT> {
    let c = &surface.chrome;
    let code = wparam.0 as u32;
    match msg {
        WM_NCMOUSEMOVE => {
            if !c.tracking.replace(true) {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: a valid structure for this window.
                let _ = unsafe { TrackMouseEvent(&mut track) };
            }
            set_hover(surface, if is_button(code) { code } else { 0 });
            None
        }
        WM_NCMOUSELEAVE => {
            c.tracking.set(false);
            c.pressed.set(0);
            set_hover(surface, 0);
            None
        }
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if is_button(code) && hidden(surface) => {
            c.pressed.set(code);
            surface.invalidate();
            Some(LRESULT(0))
        }
        WM_NCLBUTTONUP if is_button(code) && hidden(surface) => {
            if c.pressed.replace(0) == code {
                match code {
                    HTMINBUTTON => {
                        set_hover(surface, 0);
                        // SAFETY: a plain call.
                        let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
                    }
                    HTMAXBUTTON => toggle_maximize(surface),
                    _ => close(hwnd),
                }
            }
            surface.invalidate();
            Some(LRESULT(0))
        }
        _ => None,
    }
}

/// The mouse moved over the client area: off the caption buttons.
pub(crate) fn client_mouse_moved(surface: &Surface) {
    if surface.chrome.hover.get() != 0 {
        set_hover(surface, 0);
    }
}

/// `WM_NCACTIVATE`: the caption buttons dim in an inactive window. With
/// the title bar hidden, the default handling must not paint the old
/// caption over the app (`lparam` -1).
pub(crate) fn nc_activate(
    surface: &Surface,
    hwnd: HWND,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    surface.chrome.inactive.set(wparam.0 == 0);
    surface.invalidate();
    if !hidden(surface) {
        // SAFETY: default handling, then the dark menu bar's line.
        let r = unsafe { DefWindowProcW(hwnd, WM_NCACTIVATE, wparam, lparam) };
        paint_menu_bar_line(surface, hwnd);
        return Some(r);
    }
    // SAFETY: default handling, told not to repaint the frame.
    Some(unsafe { DefWindowProcW(hwnd, WM_NCACTIVATE, wparam, LPARAM(-1)) })
}

/// Draws the caption buttons over the app (with the title bar hidden).
pub(crate) fn draw(surface: &Surface, dc: &ID2D1DeviceContext, scale: f32) {
    if !hidden(surface) {
        return;
    }
    let c = &surface.chrome;
    let dark = crate::system::dark(surface);
    let hwnd = surface.hwnd();
    let width = surface.size().0;
    // Lines one device pixel wide, on pixel centers.
    let px = 1.0 / scale;
    let snap = |v: f32| (v * scale).round() / scale + px / 2.0;
    let glyph = 10.0;
    let caption = caption_height(surface);
    for (i, code) in [HTCLOSE, HTMAXBUTTON, HTMINBUTTON].into_iter().enumerate() {
        let x = width - BUTTON_WIDTH * (i + 1) as f32;
        let hovered = c.hover.get() == code;
        let pressed = hovered && c.pressed.get() == code;
        let background = match (code == HTCLOSE, hovered, pressed, dark) {
            (_, false, ..) => None,
            (true, true, false, _) => Some(Color::rgba(0xc4, 0x2b, 0x1c, 0xff)),
            (true, true, true, _) => Some(Color::rgba(0xc4, 0x2b, 0x1c, 0xe6)),
            (false, true, false, false) => Some(Color::rgba(0, 0, 0, 0x12)),
            (false, true, true, false) => Some(Color::rgba(0, 0, 0, 0x1c)),
            (false, true, false, true) => Some(Color::rgba(0xff, 0xff, 0xff, 0x14)),
            (false, true, true, true) => Some(Color::rgba(0xff, 0xff, 0xff, 0x0c)),
        };
        let foreground = if code == HTCLOSE && hovered {
            Color(0xffff_ffff)
        } else {
            let alpha = if c.inactive.get() && !hovered {
                0x70
            } else {
                0xff
            };
            if dark {
                Color::rgba(0xff, 0xff, 0xff, alpha)
            } else {
                Color::rgba(0, 0, 0, alpha)
            }
        };
        // SAFETY: drawing on the context between its BeginDraw / EndDraw.
        unsafe {
            if let Some(bg) = background
                && let Ok(brush) = dc.CreateSolidColorBrush(&color_f(bg), None)
            {
                dc.FillRectangle(&rect_f(x, 0.0, BUTTON_WIDTH, caption), &brush);
            }
            let Ok(brush) = dc.CreateSolidColorBrush(&color_f(foreground), None) else {
                continue;
            };
            let left = snap(x + (BUTTON_WIDTH - glyph) / 2.0);
            let top = snap((caption - glyph) / 2.0);
            let (right, bottom) = (left + glyph - px, top + glyph - px);
            let line = |a: (f32, f32), b: (f32, f32)| {
                dc.DrawLine(
                    Vector2 { X: a.0, Y: a.1 },
                    Vector2 { X: b.0, Y: b.1 },
                    &brush,
                    px,
                    None,
                );
            };
            match code {
                HTMINBUTTON => {
                    let y = snap(caption / 2.0);
                    line((left, y), (right + px, y));
                }
                HTMAXBUTTON if zoomed(hwnd) => {
                    // Restore: a square with another behind it.
                    let o = 2.0;
                    dc.DrawRectangle(
                        &rect_f(left, top + o, glyph - o - px, glyph - o - px),
                        &brush,
                        px,
                        None,
                    );
                    line((left + o, top + o), (left + o, top));
                    line((left + o, top), (right, top));
                    line((right, top), (right, bottom - o));
                    line((right, bottom - o), (right - o, bottom - o));
                }
                HTMAXBUTTON => {
                    dc.DrawRectangle(&rect_f(left, top, glyph - px, glyph - px), &brush, px, None);
                }
                _ => {
                    line((left, top), (right + px, bottom + px));
                    line((right + px, top), (left, bottom + px));
                }
            }
        }
    }
}

// --- Menus ------------------------------------------------------------------------------

type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type FlushMenuThemes = unsafe extern "system" fn();

/// Themes the process' menus dark (or light): uxtheme's undocumented
/// `SetPreferredAppMode` (ordinal 135, Windows 10 1903 and later) and
/// `FlushMenuThemes` (136).
fn theme_menus(dark: bool) {
    thread_local! {
        static FUNCTIONS: Option<(SetPreferredAppMode, FlushMenuThemes)> = {
            // SAFETY: uxtheme stays loaded; the ordinals have these
            // signatures on every Windows version that exports them.
            unsafe {
                LoadLibraryW(w!("uxtheme.dll")).ok().and_then(|module| {
                    let set = GetProcAddress(module, PCSTR(135 as *const u8))?;
                    let flush = GetProcAddress(module, PCSTR(136 as *const u8))?;
                    Some((
                        std::mem::transmute::<unsafe extern "system" fn() -> isize, SetPreferredAppMode>(set),
                        std::mem::transmute::<unsafe extern "system" fn() -> isize, FlushMenuThemes>(flush),
                    ))
                })
            }
        };
        static DARK: Cell<Option<bool>> = const { Cell::new(None) };
    }
    if DARK.with(|d| d.replace(Some(dark))) == Some(dark) {
        return;
    }
    FUNCTIONS.with(|f| {
        if let Some((set, flush)) = f {
            // SAFETY: the functions loaded above; 2 forces dark, 3 light.
            unsafe {
                set(if dark { 2 } else { 3 });
                flush();
            }
        }
    });
}

/// A menu item's text: `&` shows as itself, the shortcut after a tab.
fn item_text(action: &MenuAction) -> Vec<u16> {
    let label = if action.label.is_empty() {
        action.role.map_or("", MenuRole::label)
    } else {
        &action.label
    };
    let mut text = label.replace('&', "&&");
    if let Some(shortcut) = &action.shortcut {
        text.push('\t');
        text.push_str(&shortcut.label());
    }
    text.encode_utf16().chain(Some(0)).collect()
}

/// The roles Windows has no use for.
fn unsupported(role: Option<MenuRole>) -> bool {
    matches!(
        role,
        Some(MenuRole::About | MenuRole::Hide | MenuRole::HideOthers | MenuRole::ShowAll)
    )
}

/// Appends `items` to `menu`, numbering their commands in `entries`
/// (command id = index + 1).
fn append(menu: HMENU, items: &[MenuItem], entries: &mut Vec<Entry>) {
    let mut last_separator = true;
    for item in items {
        // SAFETY: appending to a live menu with NUL-terminated strings; a
        // submenu belongs to its parent once appended.
        unsafe {
            match item {
                MenuItem::Separator => {
                    if !last_separator {
                        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                        last_separator = true;
                    }
                }
                MenuItem::Action(action) => {
                    if unsupported(action.role) {
                        continue;
                    }
                    entries.push(Entry {
                        id: action.id,
                        role: action.role,
                    });
                    let mut flags = MF_STRING;
                    if !action.enabled {
                        flags |= MF_GRAYED;
                    }
                    if action.checked == Some(true) {
                        flags |= MF_CHECKED;
                    }
                    let text = item_text(action);
                    let _ = AppendMenuW(menu, flags, entries.len(), PCWSTR(text.as_ptr()));
                    last_separator = false;
                }
                MenuItem::Submenu {
                    label,
                    enabled,
                    items,
                } => {
                    let Ok(sub) = CreatePopupMenu() else { continue };
                    append(sub, items, entries);
                    let mut flags = MF_POPUP | MF_STRING;
                    if !enabled {
                        flags |= MF_GRAYED;
                    }
                    let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
                    let _ = AppendMenuW(menu, flags, sub.0 as usize, PCWSTR(text.as_ptr()));
                    last_separator = false;
                }
            }
        }
    }
}

/// Shows the application menu as the window's menu bar, unless the title
/// bar is hidden or the window is in full screen.
fn show_menu_bar(surface: &Surface) {
    let hwnd = surface.hwnd();
    let c = &surface.chrome;
    let menu = c.menu.borrow().clone();
    let shown = menu.filter(|_| {
        c.style.get() == TitleBarStyle::Default && !crate::window::is_full_screen(hwnd)
    });
    let mut entries = Vec::new();
    // SAFETY: menus made, set and destroyed on this thread; the old bar is
    // destroyed once the window no longer shows it.
    unsafe {
        let bar = match &shown {
            Some(items) => CreateMenu()
                .ok()
                .inspect(|bar| append(*bar, items, &mut entries)),
            None => None,
        };
        theme_menus(crate::system::dark(surface));
        let _ = SetMenu(hwnd, bar);
        let old = c.bar.replace(bar.map_or(0, |b| b.0 as isize));
        if old != 0 {
            let _ = DestroyMenu(HMENU(old as *mut _));
        }
        let _ = DrawMenuBar(hwnd);
    }
    // The bar's entries; the shortcuts are matched on the whole menu.
    *c.entries.borrow_mut() = entries;
}

/// `WM_COMMAND` from the menu bar.
pub(crate) fn menu_command(surface: &Rc<Surface>, command: u16) {
    let entry = surface
        .chrome
        .entries
        .borrow()
        .get((command as usize).wrapping_sub(1))
        .copied();
    if let Some(entry) = entry {
        choose(surface, entry.id, entry.role);
    }
}

/// An item was chosen: the platform carries out its role, the app hears
/// about the rest.
fn choose(surface: &Rc<Surface>, id: u32, role: Option<MenuRole>) {
    let hwnd = surface.hwnd();
    match role {
        Some(MenuRole::Quit | MenuRole::Close) => close(hwnd),
        Some(MenuRole::Minimize) => {
            // SAFETY: a plain call.
            let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
        }
        Some(MenuRole::Zoom) => toggle_maximize(surface),
        Some(MenuRole::ToggleFullScreen) => toggle_fullscreen(surface),
        Some(role) if role.edits_text() => {
            // The focused field's own keys.
            let (vk, shift) = match role {
                MenuRole::Undo => (0x5A, false),
                MenuRole::Redo => (0x59, false),
                MenuRole::Cut => (0x58, false),
                MenuRole::Copy => (0x43, false),
                MenuRole::Paste => (0x56, false),
                _ => (0x41, false),
            };
            crate::input::key(
                surface,
                KeyEvent {
                    vk,
                    down: true,
                    repeat: false,
                    modifiers: Modifiers {
                        ctrl: true,
                        shift,
                        ..Default::default()
                    },
                },
            );
        }
        _ => {}
    }
    if id != 0 {
        surface.event(
            ROOT_ID,
            EventKind::Menu,
            EventPayload::Map(vec![("id", PayloadValue::Number(id as f64))]),
        );
    }
}

/// `WM_NSH_MENU`: shows the context menu asked for.
pub(crate) fn show_popup(surface: &Rc<Surface>) {
    let Some(Popup { items, at, actions }) = surface.chrome.popup.borrow_mut().take() else {
        return;
    };
    let hwnd = surface.hwnd();
    let mut entries = Vec::new();
    // SAFETY: a menu made, tracked and destroyed on this thread.
    let chosen = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        append(menu, &items, &mut entries);
        let mut p = POINT::default();
        match at {
            Some((x, y)) => {
                let scale = surface.scale();
                p.x = (x * scale).round() as i32;
                p.y = (y * scale).round() as i32;
                let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut p);
            }
            None => {
                let _ = GetCursorPos(&mut p);
            }
        }
        theme_menus(crate::system::dark(surface));
        // The window must be in front for the menu to close when clicking
        // elsewhere.
        let _ = SetForegroundWindow(hwnd);
        #[cfg(feature = "dev")]
        if actions.is_empty() {
            *surface.chrome.tracked_popup.borrow_mut() = Some(items.clone());
        }
        let chosen = TrackPopupMenuEx(
            menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN).0,
            p.x,
            p.y,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        chosen.0 as usize
    };
    #[cfg(feature = "dev")]
    {
        surface.chrome.tracked_popup.borrow_mut().take();
        if let Some((id, role)) = surface.chrome.test_choice.take() {
            choose(surface, id, role);
        }
    }
    if let Some(entry) = entries.get(chosen.wrapping_sub(1)).copied() {
        match actions.get((entry.id as usize).wrapping_sub(1)) {
            Some(action) => action(surface),
            None => choose(surface, entry.id, entry.role),
        }
    }
    // The app's `Menu.popup` promise settles.
    if actions.is_empty() {
        surface.event(
            ROOT_ID,
            EventKind::Menu,
            EventPayload::Map(vec![
                ("id", PayloadValue::Number(0.0)),
                ("closed", PayloadValue::Bool(true)),
            ]),
        );
    }
}

/// The shortcut key of virtual key `vk`, if it can be one.
fn shortcut_key(vk: u16) -> Option<Key> {
    let k = VIRTUAL_KEY(vk);
    Some(match vk {
        0x30..=0x39 | 0x41..=0x5A => Key::Char((vk as u8 as char).to_ascii_lowercase()),
        _ if (VK_F1.0..=VK_F24.0).contains(&vk) => Key::F((vk - VK_F1.0 + 1) as u8),
        _ => match k {
            VK_RETURN => Key::Enter,
            VK_ESCAPE => Key::Escape,
            VK_TAB => Key::Tab,
            VK_BACK => Key::Backspace,
            VK_DELETE => Key::Delete,
            VK_INSERT => Key::Insert,
            VK_SPACE => Key::Space,
            VK_UP => Key::Up,
            VK_DOWN => Key::Down,
            VK_LEFT => Key::Left,
            VK_RIGHT => Key::Right,
            VK_HOME => Key::Home,
            VK_END => Key::End,
            VK_PRIOR => Key::PageUp,
            VK_NEXT => Key::PageDown,
            VK_OEM_PLUS => Key::Char('='),
            VK_OEM_MINUS => Key::Char('-'),
            VK_OEM_COMMA => Key::Char(','),
            VK_OEM_PERIOD => Key::Char('.'),
            VK_OEM_1 => Key::Char(';'),
            VK_OEM_2 => Key::Char('/'),
            VK_OEM_3 => Key::Char('`'),
            VK_OEM_4 => Key::Char('['),
            VK_OEM_5 => Key::Char('\\'),
            VK_OEM_6 => Key::Char(']'),
            VK_OEM_7 => Key::Char('\''),
            _ => return None,
        },
    })
}

/// A key pressed: the application menu's item with that shortcut is
/// chosen (true), before the app gets the key.
pub(crate) fn shortcut(surface: &Rc<Surface>, event: &KeyEvent) -> bool {
    let Some(key) = shortcut_key(event.vk) else {
        return false;
    };
    let m = event.modifiers;
    let pressed = Shortcut {
        key,
        ctrl: m.ctrl,
        alt: m.alt,
        shift: m.shift,
        meta: m.meta,
    };
    let found = {
        let menu = surface.chrome.menu.borrow();
        let Some(items) = menu.as_deref() else {
            return false;
        };
        // `plus` is the `=` key, which `shortcut_key` reads.
        MenuItem::find_shortcut(items, &pressed)
            .filter(|a| !unsupported(a.role))
            .map(|a| (a.id, a.role))
    };
    match found {
        Some((id, role)) => {
            choose(surface, id, role);
            true
        }
        None => false,
    }
}

// --- The dark menu bar ------------------------------------------------------------------

fn colorref(c: u32) -> COLORREF {
    COLORREF(c)
}

/// The menu bar's background and text in the dark theme (BGR).
const BAR_DARK: u32 = 0x002b_2b2b;
const BAR_HOT: u32 = 0x0041_4141;
const BAR_TEXT: u32 = 0x00ff_ffff;
const BAR_TEXT_DISABLED: u32 = 0x0080_8080;

/// The menu bar's rectangle in the window's coordinates.
fn menu_bar_rect(hwnd: HWND) -> Option<RECT> {
    let mut info = MENUBARINFO {
        cbSize: std::mem::size_of::<MENUBARINFO>() as u32,
        ..Default::default()
    };
    let mut window = RECT::default();
    // SAFETY: plain queries with local out values.
    unsafe {
        GetMenuBarInfo(hwnd, OBJID_MENU, 0, &mut info).ok()?;
        GetWindowRect(hwnd, &mut window).ok()?;
    }
    let mut r = info.rcBar;
    r.left -= window.left;
    r.right -= window.left;
    r.top -= window.top;
    r.bottom -= window.top;
    Some(r)
}

/// The menu bar's messages in the dark theme (`WM_UAHDRAWMENU`,
/// `WM_UAHDRAWMENUITEM`); `None` in the light theme, whose bar is the
/// system's.
pub(crate) fn draw_menu_bar(
    surface: &Surface,
    hwnd: HWND,
    msg: u32,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if !crate::system::dark(surface) || surface.chrome.bar.get() == 0 {
        return None;
    }
    match msg {
        WM_UAHDRAWMENU => {
            // SAFETY: the message's documented-by-use structure, valid for
            // the message; GDI calls on its device context.
            unsafe {
                let menu = &*(lparam.0 as *const UahMenu);
                let mut r = menu_bar_rect(hwnd)?;
                // The bar's own rectangle stops short of the window's right
                // border.
                r.right += 4;
                let brush = CreateSolidBrush(colorref(BAR_DARK));
                FillRect(menu.hdc, &r, brush);
                let _ = DeleteObject(brush.into());
            }
            Some(LRESULT(1))
        }
        WM_UAHDRAWMENUITEM => {
            // SAFETY: as above.
            unsafe {
                let item = &*(lparam.0 as *const UahDrawMenuItem);
                let mut text = [0u16; 256];
                let mut info = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_STRING,
                    dwTypeData: windows::core::PWSTR(text.as_mut_ptr()),
                    cch: text.len() as u32 - 1,
                    ..Default::default()
                };
                let _ = GetMenuItemInfoW(item.um.hmenu, item.position as u32, true, &mut info);
                let state = item.dis.itemState;
                let hot = state.0 & (ODS_HOTLIGHT.0 | ODS_SELECTED.0) != 0;
                let disabled = state.0 & (ODS_GRAYED.0 | ODS_DISABLED.0 | ODS_INACTIVE.0) != 0;
                let brush = CreateSolidBrush(colorref(if hot { BAR_HOT } else { BAR_DARK }));
                FillRect(item.um.hdc, &item.dis.rcItem, brush);
                let _ = DeleteObject(brush.into());
                SetBkMode(item.um.hdc, TRANSPARENT);
                SetTextColor(
                    item.um.hdc,
                    colorref(if disabled {
                        BAR_TEXT_DISABLED
                    } else {
                        BAR_TEXT
                    }),
                );
                let mut flags = DT_CENTER | DT_SINGLELINE | DT_VCENTER;
                if state.0 & ODS_NOACCEL.0 != 0 {
                    flags |= DT_HIDEPREFIX;
                }
                let len = text.iter().position(|&u| u == 0).unwrap_or(0);
                let mut r = item.dis.rcItem;
                DrawTextW(item.um.hdc, &mut text[..len], &mut r, flags);
            }
            Some(LRESULT(1))
        }
        _ => None,
    }
}

/// Paints over the light line the system draws under the menu bar (after
/// `WM_NCPAINT` and `WM_NCACTIVATE`), in the dark theme.
pub(crate) fn paint_menu_bar_line(surface: &Surface, hwnd: HWND) {
    if !crate::system::dark(surface) || surface.chrome.bar.get() == 0 {
        return;
    }
    let Some(bar) = menu_bar_rect(hwnd) else {
        return;
    };
    let mut client = RECT::default();
    // SAFETY: plain GDI calls on the window's own DC, released below.
    unsafe {
        let _ = GetClientRect(hwnd, &mut client);
        let line = RECT {
            left: bar.left,
            top: bar.bottom,
            right: bar.left + client.right,
            bottom: bar.bottom + 1,
        };
        let dc = GetWindowDC(Some(hwnd));
        let brush = CreateSolidBrush(colorref(BAR_DARK));
        FillRect(dc, &line, brush);
        let _ = DeleteObject(brush.into());
        ReleaseDC(Some(hwnd), dc);
    }
}

/// The appearance changed: menus and the bar follow it.
pub(crate) fn appearance_changed(surface: &Surface) {
    theme_menus(crate::system::dark(surface));
    let hwnd = surface.hwnd();
    if surface.chrome.bar.get() != 0 && !hwnd.is_invalid() {
        // SAFETY: a plain call.
        let _ = unsafe { DrawMenuBar(hwnd) };
    }
    surface.invalidate();
}

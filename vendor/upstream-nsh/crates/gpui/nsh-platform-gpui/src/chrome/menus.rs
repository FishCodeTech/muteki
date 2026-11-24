//! The drawn menus: the menu bar (Windows and Linux) and popups (context
//! menus, the bar's menus and their submenus), drawn with GPUI's elements
//! over the window.
//!
//! An open menu is a stack of levels (the menu, then the submenus open
//! from it), each laid out once when first drawn ([`measure`], [`place`]:
//! the item heights are fixed, the width fits the widest label and
//! shortcut, and a level that does not fit the window is moved or flipped
//! into it). The levels are drawn in a layer over the whole window
//! (`deferred` + `anchored`, window coordinates) that takes every mouse
//! event while a menu is open, so the app under it gets none: moves
//! highlight items and open submenus, a release on an item chooses it, a
//! press outside the menus closes them (over the menu bar, another of its
//! menus opens instead). The keyboard works the open menu from the
//! surface's window shortcuts ([`key`]): Up / Down / Home / End move,
//! Right / Left open and close submenus (or go to the bar's next and
//! previous menus), Return / Space choose, Escape closes a level, a
//! letter highlights the next item starting with it.

use std::{rc::Rc, sync::Arc};

use gpui::{
    AnchoredPositionMode, AnyElement, Bounds, CursorStyle, MouseButton, Pixels, TextRun, Window,
    anchored, canvas, deferred, div, point, prelude::*, px,
};
use nsh_core::{
    EventKind, EventPayload, PayloadValue, ROOT_ID,
    desktop::{Key, MenuAction, MenuItem, MenuRole, Shortcut},
};

use super::{Action, PLATFORM, Platform, Theme};
use crate::{input::KeyEvent, keys, surface::Surface};

/// The menus' text size.
const FONT_SIZE: f32 = 13.0;
/// An item's and a separator's height.
const ITEM_HEIGHT: f32 = 28.0;
const SEPARATOR_HEIGHT: f32 = 9.0;
/// Space above the first item and below the last.
const PADDING: f32 = 4.0;
/// The panel's border.
const BORDER: f32 = 1.0;
/// How far items are inset from the panel's sides.
const INSET: f32 = 4.0;
/// Where the label starts in an item (the check mark is left of it), the
/// space between the label and the shortcut, and what is right of the
/// shortcut (more for a submenu's arrow).
const LABEL_X: f32 = 28.0;
const SHORTCUT_GAP: f32 = 32.0;
const RIGHT: f32 = 16.0;
const ARROW: f32 = 24.0;
const MIN_WIDTH: f32 = 160.0;
/// The menu bar's height and the horizontal padding of its items.
const BAR_HEIGHT: f32 = 28.0;
const BAR_ITEM_PADDING: f32 = 10.0;

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

/// What an open menu is.
pub(super) enum Kind {
    /// The menu bar's menu at this index ([`bar_entries`]).
    Bar(usize),
    /// The app's context menu (`Menu.popup`): told when it closes.
    App,
    /// A menu of the platform's own: what its items do, by id - 1.
    Local(Vec<Action>),
}

/// Where a menu opens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Anchor {
    /// At a point (window coordinates).
    Point(f32, f32),
    /// At the mouse pointer.
    Pointer,
    /// Below the menu bar's item at this index.
    Bar(usize),
    /// Beside the item (level, row) whose submenu it is.
    Item(usize, usize),
}

/// A rectangle (window coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn contains(&self, (x, y): (f32, f32)) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    fn of(b: &Bounds<Pixels>) -> Rect {
        Rect {
            x: f32::from(b.origin.x),
            y: f32::from(b.origin.y),
            w: f32::from(b.size.width),
            h: f32::from(b.size.height),
        }
    }
}

/// A level's layout: the panel (window coordinates) and each item's top
/// and height in it.
#[derive(Debug, Clone, PartialEq)]
struct Geom {
    rect: Rect,
    rows: Vec<(f32, f32)>,
}

impl Geom {
    /// Item `row`'s rectangle in window coordinates.
    fn row(&self, row: usize) -> Rect {
        let (y, h) = self.rows[row];
        Rect {
            x: self.rect.x + INSET,
            y: self.rect.y + y,
            w: self.rect.w - 2.0 * INSET,
            h,
        }
    }
}

/// One menu of an open stack.
struct Level {
    items: Arc<[MenuItem]>,
    anchor: Anchor,
    /// Set when first drawn.
    geom: Option<Geom>,
    highlight: Option<usize>,
}

/// The open menu.
pub(super) struct Open {
    kind: Kind,
    levels: Vec<Level>,
}

/// An item's label: its own, else its role's.
pub(super) fn label(action: &MenuAction) -> String {
    if action.label.is_empty() {
        action.role.map_or("", MenuRole::label).to_owned()
    } else {
        action.label.clone()
    }
}

/// The shortcut shown for an item: its own, else its role's (Linux quits
/// with Ctrl+Q, as the standalone window does).
fn shortcut_label(action: &MenuAction) -> Option<String> {
    let mac = PLATFORM == Platform::Mac;
    let shortcut = action.shortcut.or_else(|| match action.role {
        Some(MenuRole::Quit) if PLATFORM == Platform::Linux => Some(Shortcut {
            ctrl: true,
            ..Shortcut::key(Key::Char('q'))
        }),
        role => role.and_then(|r| r.shortcut(mac)),
    })?;
    Some(shortcut.label())
}

/// The items a drawn menu shows: macOS's own roles left out elsewhere,
/// separators only between items.
fn visible(items: &[MenuItem]) -> Arc<[MenuItem]> {
    let mut out: Vec<MenuItem> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            MenuItem::Separator => {
                if out
                    .last()
                    .is_some_and(|l| !matches!(l, MenuItem::Separator))
                {
                    out.push(MenuItem::Separator);
                }
            }
            MenuItem::Action(a) if PLATFORM != Platform::Mac && super::mac_only(a.role) => {}
            item => out.push(item.clone()),
        }
    }
    if matches!(out.last(), Some(MenuItem::Separator)) {
        out.pop();
    }
    out.into()
}

/// The menu bar's items: the application menu's top-level items that show.
fn bar_entries(menu: &[MenuItem]) -> Vec<MenuItem> {
    visible(menu)
        .iter()
        .filter(|item| !matches!(item, MenuItem::Separator))
        .cloned()
        .collect()
}

/// An item can be highlighted and chosen.
fn selectable(item: &MenuItem) -> bool {
    match item {
        MenuItem::Separator => false,
        MenuItem::Action(a) => a.enabled,
        MenuItem::Submenu { enabled, .. } => *enabled,
    }
}

/// The next selectable item from `from` in direction `step` (+1 / -1),
/// wrapping around; the first (or last) one with no `from`.
fn next(items: &[MenuItem], from: Option<usize>, step: isize) -> Option<usize> {
    let n = items.len() as isize;
    if n == 0 {
        return None;
    }
    let mut i = match from {
        Some(i) => i as isize,
        None if step > 0 => -1,
        None => n,
    };
    for _ in 0..n {
        i = (i + step).rem_euclid(n);
        if selectable(&items[i as usize]) {
            return Some(i as usize);
        }
    }
    None
}

/// A level's size and its items' rows, with `width` measuring a text.
fn measure(items: &[MenuItem], width: impl Fn(&str) -> f32) -> (f32, f32, Vec<(f32, f32)>) {
    let mut y = BORDER + PADDING;
    let mut rows = Vec::with_capacity(items.len());
    let mut w = MIN_WIDTH;
    for item in items {
        let h = match item {
            MenuItem::Separator => SEPARATOR_HEIGHT,
            MenuItem::Action(a) => {
                let shortcut = shortcut_label(a).map_or(0.0, |s| SHORTCUT_GAP + width(&s));
                w = w.max(2.0 * (BORDER + INSET) + LABEL_X + width(&label(a)) + shortcut + RIGHT);
                ITEM_HEIGHT
            }
            MenuItem::Submenu { label, .. } => {
                w = w.max(2.0 * (BORDER + INSET) + LABEL_X + width(label) + ARROW);
                ITEM_HEIGHT
            }
        };
        rows.push((y, h));
        y += h;
    }
    (w.ceil(), y + PADDING + BORDER, rows)
}

/// Where an anchor resolves to (window coordinates).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Place {
    /// At a point (a context menu).
    Point(f32, f32),
    /// Below a rectangle (a menu bar's item).
    Below(Rect),
    /// Beside a rectangle (a submenu's item).
    Beside(Rect),
}

/// Where a `w` × `h` panel goes for `place` in a window `vw` × `vh`: as
/// asked if it fits, else flipped (a context menu above or left of the
/// point, a submenu left of its menu) or moved in.
fn place(place: Place, w: f32, h: f32, vw: f32, vh: f32) -> (f32, f32) {
    let fit_y = |y: f32| if y + h > vh { (vh - h).max(0.0) } else { y };
    match place {
        Place::Point(x, y) => {
            let x = if x + w <= vw { x } else { (x - w).max(0.0) };
            let y = if y + h <= vh {
                y
            } else if y - h >= 0.0 {
                y - h
            } else {
                (vh - h).max(0.0)
            };
            (x, y)
        }
        Place::Below(r) => (r.x.min(vw - w).max(0.0), fit_y(r.y + r.h)),
        Place::Beside(r) => {
            let right = r.x + r.w - 2.0;
            let x = if right + w <= vw {
                right
            } else {
                (r.x - w + 2.0).max(0.0)
            };
            (x, fit_y(r.y - PADDING - BORDER))
        }
    }
}

/// What is under a point of the window.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Hit {
    /// An item (level, row) that is not a separator.
    Row(usize, usize),
    /// A panel elsewhere than on an item.
    Panel,
    Outside,
}

/// What of the open levels is at `p` (the newest ones are on top).
fn hit(geoms: &[Option<&Geom>], p: (f32, f32)) -> Hit {
    for (l, geom) in geoms.iter().enumerate().rev() {
        let Some(g) = geom else { continue };
        if !g.rect.contains(p) {
            continue;
        }
        for (r, &(_, h)) in g.rows.iter().enumerate() {
            if h == ITEM_HEIGHT && g.row(r).contains(p) {
                return Hit::Row(l, r);
            }
        }
        return Hit::Panel;
    }
    Hit::Outside
}

// --- Opening and closing ----------------------------------------------------------------

/// Tells the app its context menu closed (`Menu.popup`'s promise
/// settles).
fn closed_event(surface: &Surface) {
    surface.event(
        ROOT_ID,
        EventKind::Menu,
        EventPayload::Map(vec![
            ("id", PayloadValue::Number(0.0)),
            ("closed", PayloadValue::Bool(true)),
        ]),
    );
}

/// Opens `items` at `anchor` (closing what was open); the keyboard opens
/// it with its first item highlighted.
pub(super) fn open(
    surface: &Surface,
    kind: Kind,
    items: Arc<[MenuItem]>,
    anchor: Anchor,
    keyboard: bool,
) {
    close(surface);
    let items = visible(&items);
    if items.is_empty() {
        if matches!(kind, Kind::App) {
            closed_event(surface);
        }
        return;
    }
    let highlight = if keyboard {
        next(&items, None, 1)
    } else {
        None
    };
    *surface.chrome.open.borrow_mut() = Some(Open {
        kind,
        levels: vec![Level {
            items,
            anchor,
            geom: None,
            highlight,
        }],
    });
    surface.invalidate();
}

/// A menu of the menu bar is open.
pub(super) fn bar_open(surface: &Surface) -> bool {
    matches!(
        surface.chrome.open.borrow().as_ref(),
        Some(Open {
            kind: Kind::Bar(_),
            ..
        })
    )
}

/// Closes the open menu, if any.
pub(super) fn close(surface: &Surface) {
    let open = surface.chrome.open.borrow_mut().take();
    if let Some(open) = open {
        if matches!(open.kind, Kind::App) {
            closed_event(surface);
        }
        surface.invalidate();
    }
}

/// The application menu changed: a menu of the bar that is open shows
/// its new items (its submenus close), or closes if it is gone.
pub(super) fn menu_changed(surface: &Surface) {
    let c = &surface.chrome;
    let index = match c.open.borrow().as_ref() {
        Some(Open {
            kind: Kind::Bar(i), ..
        }) => *i,
        _ => return,
    };
    let entry = c
        .menu
        .borrow()
        .as_deref()
        .and_then(|menu| bar_entries(menu).into_iter().nth(index));
    let Some(MenuItem::Submenu {
        enabled: true,
        items,
        ..
    }) = entry
    else {
        close(surface);
        return;
    };
    if let Some(open) = c.open.borrow_mut().as_mut() {
        let items = visible(&items);
        let highlight = open.levels[0]
            .highlight
            .filter(|&h| items.get(h).is_some_and(selectable));
        open.levels = vec![Level {
            items,
            anchor: Anchor::Bar(index),
            geom: None,
            highlight,
        }];
    }
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
                    shortcut: role.and_then(|r| r.shortcut(PLATFORM == Platform::Mac)),
                    enabled,
                    checked: None,
                    role: None,
                    when: nsh_core::keymap::When::Always,
                })
            }
        })
        .collect();
    open(
        surface,
        Kind::Local(actions),
        items.into(),
        Anchor::Pointer,
        false,
    );
}

/// Opens the menu bar's menu `index` (a top-level item that is not a menu
/// is chosen instead, from the mouse).
fn open_bar(surface: &Rc<Surface>, index: usize, keyboard: bool) -> bool {
    let entry = {
        let menu = surface.chrome.menu.borrow();
        menu.as_deref()
            .and_then(|menu| bar_entries(menu).into_iter().nth(index))
    };
    match entry {
        Some(MenuItem::Submenu {
            enabled: true,
            items,
            ..
        }) => {
            open(
                surface,
                Kind::Bar(index),
                items.into(),
                Anchor::Bar(index),
                keyboard,
            );
            true
        }
        Some(MenuItem::Action(a)) if a.enabled && !keyboard => {
            close(surface);
            super::choose(surface, a.id, a.role);
            true
        }
        _ => false,
    }
}

/// Opens the submenu of item (`level`, `row`), closing deeper levels.
fn open_submenu(open: &mut Open, level: usize, row: usize, keyboard: bool) {
    let Some(MenuItem::Submenu { items, .. }) = open.levels[level].items.get(row).cloned() else {
        return;
    };
    open.levels.truncate(level + 1);
    let items = visible(&items);
    let highlight = if keyboard {
        next(&items, None, 1)
    } else {
        None
    };
    open.levels.push(Level {
        items,
        anchor: Anchor::Item(level, row),
        geom: None,
        highlight,
    });
}

/// Chooses the item of the open menu at the end of `path` (the labels of
/// the submenus on the way, then its own), as clicks on them do (`nsh test
/// --target gpui`).
#[cfg(feature = "dev")]
pub(crate) fn choose_path(surface: &Rc<Surface>, path: &[String]) -> Result<(), String> {
    for (level, wanted) in path.iter().enumerate() {
        let row = {
            let open = surface.chrome.open.borrow();
            let open = open.as_ref().ok_or("no context menu is open")?;
            let items = &open
                .levels
                .get(level)
                .ok_or_else(|| format!("{wanted:?}'s submenu did not open"))?
                .items;
            items
                .iter()
                .position(|item| match item {
                    MenuItem::Action(action) => label(action) == *wanted,
                    MenuItem::Submenu { label, .. } => label == wanted,
                    MenuItem::Separator => false,
                })
                .ok_or_else(|| format!("the menu has no item {wanted:?}"))?
        };
        activate(surface, level, row, false);
    }
    Ok(())
}

/// Item (`level`, `row`) is chosen: a submenu opens, an action closes the
/// menu and is carried out.
fn activate(surface: &Rc<Surface>, level: usize, row: usize, keyboard: bool) {
    let item = {
        let open = surface.chrome.open.borrow();
        let Some(open) = open.as_ref() else { return };
        open.levels
            .get(level)
            .and_then(|l| l.items.get(row))
            .cloned()
    };
    match item {
        Some(MenuItem::Submenu { enabled: true, .. }) => {
            if let Some(open) = surface.chrome.open.borrow_mut().as_mut() {
                open_submenu(open, level, row, keyboard);
            }
            surface.invalidate();
        }
        Some(MenuItem::Action(a)) if a.enabled => {
            let Some(open) = surface.chrome.open.borrow_mut().take() else {
                return;
            };
            surface.invalidate();
            match open.kind {
                Kind::Local(actions) => {
                    if let Some(action) = actions.get((a.id as usize).wrapping_sub(1)) {
                        action(surface);
                    }
                }
                Kind::App => {
                    super::choose(surface, a.id, a.role);
                    closed_event(surface);
                }
                Kind::Bar(_) => super::choose(surface, a.id, a.role),
            }
        }
        _ => {}
    }
}

// --- Keys -------------------------------------------------------------------------------

/// A key down while a menu is open (all of them are the menu's), or F10
/// opening the menu bar's first menu.
pub(super) fn key(surface: &Rc<Surface>, event: &KeyEvent) -> bool {
    let c = &surface.chrome;
    let m = event.modifiers;
    if c.open.borrow().is_none() {
        let plain = !m.ctrl && !m.alt && !m.shift && !m.meta;
        return event.vk == VK_F10
            && plain
            && super::bar_available(surface)
            && open_bar(surface, 0, true);
    }
    let (kind_bar, level, highlight, items) = {
        let open = c.open.borrow();
        let Some(open) = open.as_ref() else {
            return false;
        };
        let level = open.levels.len() - 1;
        let l = &open.levels[level];
        let bar = match open.kind {
            Kind::Bar(i) => Some(i),
            _ => None,
        };
        (bar, level, l.highlight, l.items.clone())
    };
    let set_highlight = |h: Option<usize>| {
        if let Some(open) = c.open.borrow_mut().as_mut() {
            open.levels[level].highlight = h;
        }
        surface.invalidate();
    };
    let bar_count = || {
        c.menu
            .borrow()
            .as_deref()
            .map_or(0, |menu| bar_entries(menu).len())
    };
    match event.vk {
        keys::VK_ESCAPE => {
            let mut open = c.open.borrow_mut();
            match open.as_mut() {
                Some(o) if o.levels.len() > 1 => {
                    o.levels.pop();
                    drop(open);
                    surface.invalidate();
                }
                _ => {
                    drop(open);
                    close(surface);
                }
            }
        }
        keys::VK_UP => set_highlight(next(&items, highlight, -1)),
        keys::VK_DOWN => set_highlight(next(&items, highlight, 1)),
        keys::VK_HOME => set_highlight(next(&items, None, 1)),
        keys::VK_END => set_highlight(next(&items, None, -1)),
        keys::VK_RIGHT => {
            let submenu = highlight
                .filter(|&h| matches!(items.get(h), Some(MenuItem::Submenu { enabled: true, .. })));
            if let Some(row) = submenu {
                activate(surface, level, row, true);
            } else if let Some(i) = kind_bar {
                let n = bar_count();
                if n > 0 {
                    open_bar(surface, (i + 1) % n, true);
                }
            }
        }
        keys::VK_LEFT => {
            if level > 0 {
                if let Some(open) = c.open.borrow_mut().as_mut() {
                    open.levels.pop();
                }
                surface.invalidate();
            } else if let Some(i) = kind_bar {
                let n = bar_count();
                if n > 0 {
                    open_bar(surface, (i + n - 1) % n, true);
                }
            }
        }
        keys::VK_RETURN | keys::VK_SPACE => {
            if let Some(row) = highlight {
                activate(surface, level, row, true);
            }
        }
        vk @ (0x30..=0x39 | 0x41..=0x5A) if !m.ctrl && !m.meta => {
            // The next item starting with that letter.
            let letter = (vk as u8 as char).to_ascii_lowercase();
            let starts = |i: usize| {
                let text = match &items[i] {
                    MenuItem::Action(a) => label(a),
                    MenuItem::Submenu { label, .. } => label.clone(),
                    MenuItem::Separator => return false,
                };
                selectable(&items[i])
                    && text
                        .chars()
                        .next()
                        .is_some_and(|c| c.to_ascii_lowercase() == letter)
            };
            let n = items.len();
            let from = highlight.map_or(0, |h| h + 1);
            if let Some(i) = (0..n).map(|k| (from + k) % n).find(|&i| starts(i)) {
                set_highlight(Some(i));
            }
        }
        _ => {}
    }
    true
}

/// `VK_F10`.
const VK_F10: u16 = 0x79;

/// Alt + a letter opens the menu bar's menu starting with it.
pub(super) fn mnemonic(surface: &Rc<Surface>, pressed: &Shortcut) -> bool {
    let Key::Char(letter) = pressed.key else {
        return false;
    };
    if !pressed.alt
        || pressed.ctrl
        || pressed.meta
        || pressed.shift
        || !super::bar_available(surface)
    {
        return false;
    }
    let index = {
        let menu = surface.chrome.menu.borrow();
        menu.as_deref().and_then(|menu| {
            bar_entries(menu).iter().position(|item| {
                let text = match item {
                    MenuItem::Action(a) => label(a),
                    MenuItem::Submenu { label, .. } => label.clone(),
                    MenuItem::Separator => String::new(),
                };
                text.chars()
                    .next()
                    .is_some_and(|c| c.to_ascii_lowercase() == letter)
            })
        })
    };
    index.is_some_and(|i| open_bar(surface, i, true))
}

// --- The mouse --------------------------------------------------------------------------

/// The menu bar's item at `p`, if any.
fn bar_item_at(surface: &Surface, p: (f32, f32)) -> Option<usize> {
    surface
        .chrome
        .bar_items
        .borrow()
        .iter()
        .position(|b| Rect::of(b).contains(p))
}

fn hit_open(open: &Open, p: (f32, f32)) -> Hit {
    let geoms: Vec<Option<&Geom>> = open.levels.iter().map(|l| l.geom.as_ref()).collect();
    hit(&geoms, p)
}

/// The mouse moved over the menu layer.
fn mouse_moved(surface: &Rc<Surface>, p: (f32, f32)) {
    let c = &surface.chrome;
    let mut switch_to = None;
    {
        let mut open = c.open.borrow_mut();
        let Some(open) = open.as_mut() else { return };
        match hit_open(open, p) {
            Hit::Row(l, r) => {
                let item = open.levels[l].items[r].clone();
                if !selectable(&item) {
                    if open.levels[l].highlight.take().is_some() {
                        surface.invalidate();
                    }
                    return;
                }
                if open.levels[l].highlight.replace(r) != Some(r) {
                    surface.invalidate();
                }
                let shown = open
                    .levels
                    .get(l + 1)
                    .is_some_and(|sub| sub.anchor == Anchor::Item(l, r));
                if matches!(item, MenuItem::Submenu { .. }) {
                    if !shown {
                        open_submenu(open, l, r, false);
                        surface.invalidate();
                    }
                } else if open.levels.len() > l + 1 {
                    open.levels.truncate(l + 1);
                    surface.invalidate();
                }
            }
            Hit::Panel => {}
            Hit::Outside => {
                if let Kind::Bar(i) = open.kind
                    && let Some(j) = bar_item_at(surface, p)
                    && j != i
                {
                    switch_to = Some(j);
                }
            }
        }
    }
    if let Some(j) = switch_to {
        open_bar(surface, j, false);
    }
}

/// A button went down over the menu layer.
fn mouse_pressed(surface: &Rc<Surface>, p: (f32, f32)) {
    let (outside, bar) = {
        let open = surface.chrome.open.borrow();
        let Some(open) = open.as_ref() else { return };
        let bar = match open.kind {
            Kind::Bar(i) => Some(i),
            _ => None,
        };
        (hit_open(open, p) == Hit::Outside, bar)
    };
    if !outside {
        return;
    }
    match (bar, bar_item_at(surface, p)) {
        (Some(i), Some(j)) if j != i => {
            open_bar(surface, j, false);
        }
        _ => close(surface),
    }
}

/// A button went up over the menu layer: the item under it is chosen.
fn mouse_released(surface: &Rc<Surface>, p: (f32, f32)) {
    let found = {
        let open = surface.chrome.open.borrow();
        let Some(open) = open.as_ref() else { return };
        hit_open(open, p)
    };
    if let Hit::Row(l, r) = found {
        activate(surface, l, r, false);
    }
}

// --- Drawing ----------------------------------------------------------------------------

/// The width of `text` in the menus' font.
fn text_width(window: &Window, text: &str) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let run = TextRun {
        len: text.len(),
        font: gpui::font(crate::text::ui_family().to_string()),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .layout_line(text, px(FONT_SIZE), &[run], None);
    f32::from(line.width)
}

/// Ends a mouse handler of the menus: the event stops here and the
/// surfaces that changed redraw.
fn handled(cx: &mut gpui::App) {
    cx.stop_propagation();
    crate::rt::flush(cx);
}

/// The menu bar (Windows and Linux).
pub(super) fn bar(surface: &Rc<Surface>, _window: &mut Window, theme: &Theme) -> AnyElement {
    let entries = surface
        .chrome
        .menu
        .borrow()
        .as_deref()
        .map(bar_entries)
        .unwrap_or_default();
    surface
        .chrome
        .bar_items
        .borrow_mut()
        .resize(entries.len(), Bounds::default());
    let open = match surface.chrome.open.borrow().as_ref() {
        Some(Open {
            kind: Kind::Bar(i), ..
        }) => Some(*i),
        _ => None,
    };
    let mut bar = div()
        .flex_none()
        .w_full()
        .h(px(BAR_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .px(px(4.0))
        .bg(theme.bar)
        .border_b_1()
        .border_color(theme.bar_border)
        .text_size(px(FONT_SIZE));
    for (i, entry) in entries.iter().enumerate() {
        let (text, enabled) = match entry {
            MenuItem::Action(a) => (label(a), a.enabled),
            MenuItem::Submenu { label, enabled, .. } => (label.clone(), *enabled),
            MenuItem::Separator => continue,
        };
        let s = surface.clone();
        let record = surface.clone();
        let highlight = theme.highlight;
        bar = bar.child(
            div()
                .id(("nsh-menu-bar", i))
                .relative()
                .h(px(BAR_HEIGHT - 4.0))
                .px(px(BAR_ITEM_PADDING))
                .flex()
                .items_center()
                .rounded(px(4.0))
                .whitespace_nowrap()
                .text_color(if enabled { theme.text } else { theme.disabled })
                .when(open == Some(i), |d| d.bg(highlight))
                .when(enabled, |d| d.hover(move |s| s.bg(highlight)))
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    open_bar(&s, i, false);
                    handled(cx);
                })
                .child(text)
                .child(
                    canvas(
                        move |bounds, _, _| {
                            if let Some(b) = record.chrome.bar_items.borrow_mut().get_mut(i) {
                                *b = bounds;
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                ),
        );
    }
    bar.into_any_element()
}

/// Lays out the levels drawn for the first time.
fn lay_out(surface: &Surface, open: &mut Open, window: &mut Window) {
    let viewport = window.viewport_size();
    let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
    for i in 0..open.levels.len() {
        if open.levels[i].geom.is_some() {
            continue;
        }
        let (w, h, rows) = measure(&open.levels[i].items, |t| text_width(window, t));
        let at = match open.levels[i].anchor {
            Anchor::Point(x, y) => Place::Point(x, y),
            Anchor::Pointer => {
                // Just off the pointer: the release of the press that
                // opened the menu must not choose an item.
                let p = window.mouse_position();
                Place::Point(f32::from(p.x) + 1.0, f32::from(p.y) + 1.0)
            }
            Anchor::Bar(j) => {
                let item = surface.chrome.bar_items.borrow().get(j).map(Rect::of);
                match item {
                    Some(item) if item.w > 0.0 => Place::Below(item),
                    // The bar was hidden: it is placed in this frame, the
                    // menu below it in the next.
                    _ => {
                        window.refresh();
                        continue;
                    }
                }
            }
            Anchor::Item(l, r) => match open.levels.get(l).and_then(|l| l.geom.as_ref()) {
                Some(g) => Place::Beside(g.row(r)),
                None => Place::Point(0.0, 0.0),
            },
        };
        let (x, y) = place(at, w, h, vw, vh);
        open.levels[i].geom = Some(Geom {
            rect: Rect { x, y, w, h },
            rows,
        });
    }
}

/// A small stroked glyph (a check mark, a submenu's arrow), `w` × `h`.
fn glyph(points: &'static [(f32, f32)], w: f32, h: f32, color: gpui::Rgba) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let (x0, y0) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let mut b = gpui::PathBuilder::stroke(px(1.5));
            for (i, &(x, y)) in points.iter().enumerate() {
                let p = point(px(x0 + x * w), px(y0 + y * h));
                if i == 0 {
                    b.move_to(p);
                } else {
                    b.line_to(p);
                }
            }
            if let Ok(path) = b.build() {
                window.paint_path(path, color);
            }
        },
    )
    .w(px(w))
    .h(px(h))
}

const CHECK: &[(f32, f32)] = &[(0.0, 0.55), (0.36, 0.9), (1.0, 0.1)];
const CHEVRON: &[(f32, f32)] = &[(0.0, 0.0), (1.0, 0.5), (0.0, 1.0)];

/// One level's panel.
fn panel(level: &Level, theme: &Theme) -> Option<AnyElement> {
    let g = level.geom.as_ref()?;
    let mut panel = div()
        .absolute()
        .left(px(g.rect.x))
        .top(px(g.rect.y))
        .w(px(g.rect.w))
        .h(px(g.rect.h))
        .bg(theme.panel)
        .border_1()
        .border_color(theme.border)
        .rounded(px(6.0))
        .shadow_lg();
    for (i, item) in level.items.iter().enumerate() {
        let (y, h) = g.rows[i];
        // Children are placed in the panel's padding box, inside the
        // border.
        let top = y - BORDER;
        let left = INSET - BORDER;
        let width = g.rect.w - 2.0 * INSET;
        let (text, shortcut, checked, submenu, enabled) = match item {
            MenuItem::Separator => {
                panel = panel.child(
                    div()
                        .absolute()
                        .left(px(left + 8.0))
                        .w(px(width - 16.0))
                        .top(px(top + (h / 2.0).floor()))
                        .h(px(1.0))
                        .bg(theme.separator),
                );
                continue;
            }
            MenuItem::Action(a) => (
                label(a),
                shortcut_label(a),
                a.checked == Some(true),
                false,
                a.enabled,
            ),
            MenuItem::Submenu { label, enabled, .. } => {
                (label.clone(), None, false, true, *enabled)
            }
        };
        let color = if enabled { theme.text } else { theme.disabled };
        let mut row = div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .h(px(h))
            .rounded(px(4.0))
            .when(level.highlight == Some(i), |d| d.bg(theme.highlight))
            .child(
                div()
                    .absolute()
                    .left(px(LABEL_X))
                    .top_0()
                    .h(px(h))
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .text_color(color)
                    .child(text),
            );
        if checked {
            row = row.child(
                div()
                    .absolute()
                    .left(px(9.0))
                    .top(px((h - 8.0) / 2.0))
                    .child(glyph(CHECK, 10.0, 8.0, color)),
            );
        }
        if let Some(shortcut) = shortcut {
            row = row.child(
                div()
                    .absolute()
                    .right(px(RIGHT - 4.0))
                    .top_0()
                    .h(px(h))
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .text_color(if enabled {
                        theme.secondary
                    } else {
                        theme.disabled
                    })
                    .child(shortcut),
            );
        }
        if submenu {
            row = row.child(
                div()
                    .absolute()
                    .right(px(10.0))
                    .top(px((h - 8.0) / 2.0))
                    .child(glyph(CHEVRON, 4.0, 8.0, color)),
            );
        }
        panel = panel.child(row);
    }
    Some(panel.into_any_element())
}

/// The open menu's layer over the window, if a menu is open.
pub(super) fn layer(
    surface: &Rc<Surface>,
    window: &mut Window,
    theme: &Theme,
) -> Option<AnyElement> {
    let mut open = surface.chrome.open.borrow_mut();
    let open = open.as_mut()?;
    lay_out(surface, open, window);
    let viewport = window.viewport_size();
    let at = |e: gpui::Point<Pixels>| (f32::from(e.x), f32::from(e.y));
    let (s1, s2, s3, s4) = (
        surface.clone(),
        surface.clone(),
        surface.clone(),
        surface.clone(),
    );
    let layer = div()
        .id("nsh-menu-layer")
        .relative()
        .w(viewport.width)
        .h(viewport.height)
        .occlude()
        .cursor(CursorStyle::Arrow)
        .font_family(crate::text::ui_family().to_string())
        .text_size(px(FONT_SIZE))
        .on_any_mouse_down(move |e, _, cx| {
            mouse_pressed(&s1, at(e.position));
            handled(cx);
        })
        // Moves go on: the views under the layer see the pointer leave
        // them (their hover state ends).
        .on_mouse_move(move |e, _, cx| {
            mouse_moved(&s2, at(e.position));
            crate::rt::flush(cx);
        })
        .on_mouse_up(MouseButton::Left, move |e, _, cx| {
            mouse_released(&s3, at(e.position));
            handled(cx);
        })
        .on_mouse_up(MouseButton::Right, move |e, _, cx| {
            mouse_released(&s4, at(e.position));
            handled(cx);
        })
        .children(open.levels.iter().filter_map(|l| panel(l, theme)));
    Some(
        deferred(
            anchored()
                .position(point(px(0.0), px(0.0)))
                .position_mode(AnchoredPositionMode::Window)
                .child(layer),
        )
        .with_priority(1)
        .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use nsh_core::desktop::{MenuAction, MenuItem};

    use super::*;

    fn action(id: u32, label: &str, enabled: bool) -> MenuItem {
        MenuItem::Action(MenuAction {
            id,
            label: label.to_owned(),
            shortcut: None,
            enabled,
            checked: None,
            role: None,
            when: nsh_core::keymap::When::Always,
        })
    }

    #[test]
    fn separators_only_between_items() {
        let items = [
            MenuItem::Separator,
            action(1, "a", true),
            MenuItem::Separator,
            MenuItem::Separator,
            action(2, "b", true),
            MenuItem::Separator,
        ];
        let shown = visible(&items);
        assert_eq!(shown.len(), 3);
        assert!(matches!(shown[1], MenuItem::Separator));
    }

    #[test]
    fn keyboard_skips_separators_and_disabled_items() {
        let items = [
            action(1, "a", true),
            MenuItem::Separator,
            action(2, "b", false),
            action(3, "c", true),
        ];
        assert_eq!(next(&items, None, 1), Some(0));
        assert_eq!(next(&items, Some(0), 1), Some(3));
        assert_eq!(next(&items, Some(3), 1), Some(0));
        assert_eq!(next(&items, None, -1), Some(3));
        assert_eq!(next(&[MenuItem::Separator], None, 1), None);
    }

    #[test]
    fn layout_fits_the_widest_label() {
        let items = [
            action(1, "short", true),
            MenuItem::Separator,
            action(2, &"x".repeat(40), true),
        ];
        let (w, h, rows) = measure(&items, |t| t.len() as f32 * 7.0);
        assert_eq!(rows, vec![(5.0, 28.0), (33.0, 9.0), (42.0, 28.0)]);
        assert_eq!(h, 75.0);
        assert_eq!(w, 2.0 * (BORDER + INSET) + LABEL_X + 280.0 + RIGHT);
        let (w, ..) = measure(&items[..1], |t| t.len() as f32 * 7.0);
        assert_eq!(w, MIN_WIDTH);
    }

    #[test]
    fn panels_stay_in_the_window() {
        // Fits: where asked.
        assert_eq!(
            place(Place::Point(10.0, 20.0), 100.0, 50.0, 400.0, 300.0),
            (10.0, 20.0)
        );
        // Too far right and down: left of and above the point.
        assert_eq!(
            place(Place::Point(350.0, 280.0), 100.0, 50.0, 400.0, 300.0),
            (250.0, 230.0)
        );
        // A submenu with no room on the right opens on the left.
        let item = Rect {
            x: 300.0,
            y: 40.0,
            w: 90.0,
            h: 28.0,
        };
        let (x, y) = place(Place::Beside(item), 100.0, 50.0, 400.0, 300.0);
        assert_eq!((x, y), (202.0, 35.0));
        // A bar's menu is moved in from the right edge.
        let bar = Rect {
            x: 380.0,
            y: 0.0,
            w: 40.0,
            h: 24.0,
        };
        assert_eq!(
            place(Place::Below(bar), 100.0, 50.0, 400.0, 300.0),
            (300.0, 24.0)
        );
    }

    #[test]
    fn hits() {
        let g = Geom {
            rect: Rect {
                x: 100.0,
                y: 100.0,
                w: 200.0,
                h: 75.0,
            },
            rows: vec![(5.0, 28.0), (33.0, 9.0), (42.0, 28.0)],
        };
        let geoms = [Some(&g)];
        assert_eq!(hit(&geoms, (150.0, 110.0)), Hit::Row(0, 0));
        assert_eq!(hit(&geoms, (150.0, 136.0)), Hit::Panel);
        assert_eq!(hit(&geoms, (150.0, 150.0)), Hit::Row(0, 2));
        assert_eq!(hit(&geoms, (50.0, 150.0)), Hit::Outside);
    }
}

//! Tests in windows of GPUI's test platform (`TestAppContext`; feature
//! `shot`, which turns on GPUI's test support): a surface drawn by GPUI
//! and given GPUI's input, with the engine running on its own thread.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App as GpuiApp, Entity, Focusable, IntoElement, Styled, TestAppContext,
    VisualTestContext, Window, div,
};

use crate::{App, AppOptions, AppView, Bundle, NativeComponent, Root};

/// A native component that only draws a box.
struct Square;

impl NativeComponent for Square {
    fn render(&mut self, _: &mut Window, _: &mut GpuiApp) -> Option<AnyElement> {
        Some(div().size_full().into_any_element())
    }

    fn set_prop(
        &mut self,
        _: &str,
        _: Option<&serde_json::Value>,
        _: &mut Window,
        _: &mut GpuiApp,
    ) {
    }
}

/// A component and, after it, a focused field.
const FIELD_AFTER_COMPONENT: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";
import { nativeComponent } from "nsh";

const Square = nativeComponent<{}, {}>("test.square", { props: [], events: [] });

function App() {
  const text = signal("abcd");
  return (
    <View style={{ flex: 1, padding: 8, gap: 8 }}>
      <Square style={{ width: 40, height: 20 }} />
      <Input autoFocus value={text()} onChange={({ value }) => text.set(value)} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

/// A focused field that logs its input method's compositions and sets
/// values while they are in progress: "held" on "ni" (applied once the
/// composition is cancelled), "stale" on "hao" (dropped, as "好" is
/// committed after it); the log becomes the text when "hao" ends.
const COMPOSING_FIELD: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";

function App() {
  const text = signal("abcd");
  const log: string[] = [];
  return (
    <View style={{ flex: 1, padding: 8 }}>
      <Input
        autoFocus
        value={text()}
        onChange={({ value: t }) => {
          log.push("change " + t);
          text.set(t);
        }}
        onCompositionChange={(e) => {
          log.push(e.composing ? "compose " + e.text : "end");
          if (e.text === "ni") text.set("held");
          if (e.text === "hao") text.set("stale");
          if (!e.composing && log.length > 2) text.set(log.join("|"));
        }}
        style={{ height: 30 }}
      />
    </View>
  );
}
mount(App);
"#;

/// A host view around a surface that holds the window's focus itself,
/// as an embedding app's own widgets do.
struct Host {
    view: Entity<AppView>,
    focus: gpui::FocusHandle,
}

impl gpui::Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _};
        div()
            .size_full()
            .track_focus(&self.focus)
            .child(self.view.clone())
    }
}

/// The focused field's text.
fn field_text(view: &Entity<AppView>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |view, _| {
        crate::text_input::focused(view.surface())
            .and_then(|field| crate::text_input::accessible_value(&field))
            .map(|value| value.text)
    })
}

/// The value the focused field holds until its composition ends.
fn field_pending(view: &Entity<AppView>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |view, _| {
        crate::text_input::focused(view.surface())
            .and_then(|field| crate::text_input::pending_value(&field))
    })
}

/// Runs the window until the focused field holds `text` for after its
/// composition.
fn wait_for_pending(view: &Entity<AppView>, cx: &mut VisualTestContext, text: &str) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        let now = field_pending(view, cx);
        if now.as_deref() == Some(text) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the field holds {now:?}, not {text:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An input method's step on the focused field.
fn ime(
    view: &Entity<AppView>,
    cx: &mut VisualTestContext,
    f: impl FnOnce(&Rc<crate::surface::Surface>),
) {
    view.update(cx, |view, _| f(view.surface()));
}

/// Runs the window (and waits for the engine's thread) until the focused
/// field reads `text`.
fn wait_for(view: &Entity<AppView>, cx: &mut VisualTestContext, text: &str) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        let now = field_text(view, cx);
        if now.as_deref() == Some(text) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the field reads {now:?}, not {text:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The tests in windows, one after the other in one app: the main
/// thread's queue (`crate::rt`) is made once per process, for the first
/// app, so every window's engine must post to that app.
#[gpui::test]
fn windows(cx: &mut TestAppContext) {
    // The engine (and the settings portal) run on threads of their own.
    cx.executor().allow_parking();
    keys_reach_the_fields_of_a_surface_showing_a_native_component(cx);
    compositions_are_reported_and_hold_values_until_they_end(cx);
    menu_shortcuts_leave_a_focused_field_the_keys_it_uses(cx);
    presses_carry_where_the_pointer_was(cx);
    presses_carry_the_modifier_keys_and_click_count(cx);
    mouse_events_carry_the_position_button_and_modifiers(cx);
    an_embedded_surface_takes_the_keyboard_when_its_app_focuses_a_field(cx);
    text_typed_ahead_of_enter_or_a_binding_lands_in_the_next_field(cx);
    the_app_s_shortcuts_reach_it_while_a_native_component_has_the_keyboard(cx);
    js_and_rust_view_shortcuts_fire_together_in_either_order(cx);
    each_view_s_functions_read_the_context_its_options_gave_its_engine(cx);
    an_embedded_view_that_owns_its_window_sets_its_title_and_menu(cx);
    quit_and_close_requests_go_to_the_handler_instead_of_acting(cx);
    an_auto_hidden_menu_bar_shows_on_alt_alone_and_while_a_menu_is_open(cx);
    asset_uris_resolve_in_the_first_directory_that_has_the_file(cx);
    releasing_shift_before_alt_keeps_the_auto_hidden_menu_bar_hidden(cx);
}

fn keys_reach_the_fields_of_a_surface_showing_a_native_component(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", FIELD_AFTER_COMPONENT, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| {
        view.register_component("test.square", Rc::new(|_| Box::new(Square)));
        view.load(App::Js(Bundle::source(code)));
    });
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "abcd");
    // Painting the component's element must not take the surface's key
    // listeners with it: deletions and moves come as key events.
    cx.simulate_keystrokes("end backspace");
    wait_for(&view, cx, "abc");
    cx.simulate_keystrokes("home delete");
    wait_for(&view, cx, "bc");
    cx.simulate_keystrokes("right shift-right backspace");
    wait_for(&view, cx, "b");
}

fn compositions_are_reported_and_hold_values_until_they_end(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", COMPOSING_FIELD, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| view.load(App::Js(Bundle::source(code))));
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "abcd");
    cx.simulate_keystrokes("end");
    // The same composition twice is reported once; the value JS sets
    // meanwhile waits, the text keeps what the input method composes in.
    ime(&view, cx, |s| crate::text_input::mark(s, "ni"));
    ime(&view, cx, |s| crate::text_input::mark(s, "ni"));
    wait_for_pending(&view, cx, "held");
    assert_eq!(field_text(&view, cx).as_deref(), Some("abcd"));
    // Cancelled: the value held is applied as the composition ends.
    ime(&view, cx, |s| crate::text_input::mark(s, ""));
    assert_eq!(field_text(&view, cx).as_deref(), Some("held"));
    assert_eq!(field_pending(&view, cx), None);
    // Committed: the commit's `onChange` comes before the end, and the
    // value set before the commit is stale.
    ime(&view, cx, |s| crate::text_input::mark(s, "hao"));
    wait_for_pending(&view, cx, "stale");
    ime(&view, cx, |s| crate::text_input::char_input(s, "好"));
    assert_eq!(field_text(&view, cx).as_deref(), Some("held好"));
    wait_for(&view, cx, "compose ni|end|compose hao|change held好|end");
}

fn an_embedded_surface_takes_the_keyboard_when_its_app_focuses_a_field(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", FIELD_AFTER_COMPONENT, &[]).unwrap();
    use gpui::AppContext as _;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| AppView::new(AppOptions::default(), window, cx));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Host { view, focus }
    });
    let view = host.read_with(cx, |host, _| host.view.clone());
    view.update(cx, |view, _| {
        view.register_component("test.square", Rc::new(|_| Box::new(Square)));
        view.load(App::Js(Bundle::source(code)));
    });
    // The app's autoFocus field. The window's focus stays on the host's
    // own element until the surface takes it.
    wait_for(&view, cx, "abcd");
    cx.simulate_keystrokes("end backspace");
    wait_for(&view, cx, "abc");
    // The host can take the keyboard while the surface keeps its focused field.
    let host_focus = host.read_with(cx, |host, _| host.focus.clone());
    cx.update(|window, cx| window.focus(&host_focus, cx));
    cx.run_until_parked();
    view.update(cx, |view, _| {
        let field = crate::text_input::focused(view.surface()).unwrap();
        crate::text_input::focus(&field);
    });
    cx.run_until_parked();
    cx.update(|window, _| {
        assert!(
            !host_focus.is_focused(window),
            "focus() left the keyboard on the embedding host"
        );
    });
    cx.simulate_keystrokes("end backspace");
    wait_for(&view, cx, "ab");
}

/// Two fields. Enter in the first, and Ctrl+N (Command+N on macOS), both
/// focus the second. Text typed before the engine has handled that key
/// belongs to the second field.
const TWO_FIELDS: &str = r#"
import { keymap, mount, signal, View, type InputRef } from "nsh";
import { Input } from "nsh/base";

function App() {
  const first = signal("A");
  const second = signal("");
  let next!: InputRef;
  const go = () => next.focus();
  keymap.bind("mod+n", go, { when: "always" });
  return (
    <View style={{ flex: 1, padding: 8, gap: 8 }}>
      <Input
        autoFocus
        value={first()}
        onChange={({ value }) => first.set(value)}
        onSubmit={go}
        submitBehavior="submit"
        style={{ height: 30 }}
      />
      <Input ref={next} value={second()} onChange={({ value }) => second.set(value)} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

fn down(vk: u16, modifiers: crate::input::Modifiers) -> crate::input::KeyEvent {
    crate::input::KeyEvent {
        vk,
        down: true,
        repeat: false,
        modifiers,
    }
}

/// `x`, then Backspace, then `y`, before the engine applies `key`. The
/// focused field must still read "A", and then "y" once the next field
/// has the keyboard.
fn typed_ahead(cx: &mut TestAppContext, key: crate::input::KeyEvent) {
    let code = nsh_bundler::compile_module("App.tsx", TWO_FIELDS, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| view.load(App::Js(Bundle::source(code))));
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "A");
    view.update(cx, |view, _| {
        let surface = view.surface().clone();
        assert!(crate::input::key(&surface, key), "the key was not handled");
        crate::text_input::char_input(&surface, "x");
        assert!(crate::input::key(
            &surface,
            down(crate::keys::VK_BACK, crate::input::Modifiers::default())
        ));
        crate::text_input::char_input(&surface, "y");
        let now = crate::text_input::focused(view.surface())
            .and_then(|field| crate::text_input::accessible_value(&field))
            .map(|value| value.text);
        assert!(
            matches!(now.as_deref(), None | Some("A")),
            "typed ahead into the field that still has the keyboard: {now:?}"
        );
    });
    wait_for(&view, cx, "y");
}

fn text_typed_ahead_of_enter_or_a_binding_lands_in_the_next_field(cx: &mut TestAppContext) {
    typed_ahead(
        cx,
        down(crate::keys::VK_RETURN, crate::input::Modifiers::default()),
    );
    let modifiers = if cfg!(target_os = "macos") {
        crate::input::Modifiers {
            meta: true,
            ..crate::input::Modifiers::default()
        }
    } else {
        crate::input::Modifiers {
            ctrl: true,
            ..crate::input::Modifiers::default()
        }
    };
    typed_ahead(cx, down(0x4E, modifiers));
}

struct KeyLog {
    keys: Rc<std::cell::RefCell<Vec<String>>>,
    focus: Rc<std::cell::RefCell<Option<gpui::FocusHandle>>>,
}

impl NativeComponent for KeyLog {
    fn render(&mut self, _: &mut Window, cx: &mut GpuiApp) -> Option<AnyElement> {
        use gpui::InteractiveElement as _;
        let focus = self
            .focus
            .borrow_mut()
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let keys = self.keys.clone();
        Some(
            div()
                .size_full()
                .track_focus(&focus)
                .on_key_down(move |e, _, _| keys.borrow_mut().push(e.keystroke.unparse()))
                .into_any_element(),
        )
    }

    fn focus_handle(&self) -> Option<gpui::FocusHandle> {
        self.focus.borrow().clone()
    }

    fn set_prop(
        &mut self,
        _: &str,
        _: Option<&serde_json::Value>,
        _: &mut Window,
        _: &mut GpuiApp,
    ) {
    }
}

/// A field showing which of the app's shortcuts fired, above a component
/// that takes the keyboard. `mod+k` fires while text is edited. `j` does
/// not. The menu's `mod+z` and `mod+shift+n` still choose their items.
const SHORTCUTS_OVER_COMPONENT: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";
import { Menu } from "nsh/desktop";
import { keymap } from "nsh";
import { nativeComponent } from "nsh";

const KeyLog = nativeComponent<{}, {}>("test.keylog", { props: [], events: [] });

function App() {
  const fired = signal("fired");
  const fire = (name: string) => () => fired.set(`${fired()} ${name}`);
  keymap.bind("mod+k", fire("bound"), { when: "always" });
  keymap.bind("j", fire("j"));
  Menu.setApplicationMenu([
    {
      label: "Edit",
      submenu: [
        { label: "Revert", shortcut: "mod+z", onSelect: fire("revert") },
        { label: "New", shortcut: "mod+shift+n", onSelect: fire("new") },
      ],
    },
  ]);
  return (
    <View style={{ flex: 1 }}>
      <Input value={fired()} style={{ height: 30 }} />
      <KeyLog style={{ height: 40 }} />
    </View>
  );
}
mount(App);
"#;

fn the_app_s_shortcuts_reach_it_while_a_native_component_has_the_keyboard(cx: &mut TestAppContext) {
    use std::cell::RefCell;
    let code = nsh_bundler::compile_module("App.tsx", SHORTCUTS_OVER_COMPONENT, &[]).unwrap();
    let keys = Rc::new(RefCell::new(Vec::new()));
    let focus = Rc::new(RefCell::new(None));
    let (component_keys, component_focus) = (keys.clone(), focus.clone());
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = AppView::new(AppOptions::default(), window, cx);
        let title = window.window_title();
        crate::chrome::set_standalone(view.surface(), &title, window.window_handle());
        view
    });
    view.update(cx, |view, _| {
        view.register_component(
            "test.keylog",
            Rc::new(move |_| {
                Box::new(KeyLog {
                    keys: component_keys.clone(),
                    focus: component_focus.clone(),
                })
            }),
        );
        view.load(App::Js(Bundle::source(code)));
    });
    let start = Instant::now();
    let handle = loop {
        cx.run_until_parked();
        if let Some(handle) = focus.borrow().clone() {
            break handle;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the component is not drawn"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    cx.update(|window, cx| window.focus(&handle, cx));
    cx.run_until_parked();
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    cx.simulate_keystrokes(&format!("a j {primary}-z {primary}-k"));
    // GPUI's test window sends no releases. K's release reaches the keymap
    // while the component has the focus, so the next Ctrl+K is not a repeat.
    cx.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse(&format!("{primary}-k")).unwrap(),
    });
    cx.simulate_keystrokes(&format!("{primary}-k shift-{primary}-n"));
    assert_eq!(
        keys.borrow()[..4],
        ["a", "j", &format!("{primary}-z"), &format!("{primary}-k")],
    );
    cx.update(|window, _| assert!(handle.is_focused(window), "the component lost the focus"));
    // The surface starts below the platform's title and menu bars.
    let (x, y) = view.read_with(cx, |view, _| view.surface().origin.get());
    cx.simulate_click(
        gpui::point(gpui::px(x + 20.0), gpui::px(y + 15.0)),
        gpui::Modifiers::default(),
    );
    wait_for(&view, cx, "fired revert bound bound new");
}
/// The part of the embedding app a view reaches: its engine's context.
struct Desk(&'static str);

/// Which desk the engine running the call was given.
#[nsh_core::element::module]
struct Whose;

#[nsh_core::element::module]
impl Whose {
    /// In the JS call.
    pub(crate) fn now() -> String {
        desk()
    }

    /// As a task, after it waited.
    pub(crate) async fn later() -> String {
        nsh_core::prelude::timer(Duration::from_millis(5)).await;
        desk()
    }
}

fn desk() -> String {
    nsh_core::element::engine_context::<Desk>()
        .map_or_else(|| "none".to_owned(), |desk| desk.0.to_owned())
}

/// A field showing which desk `Whose` names, in the call and in a task.
const WHOSE: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";
import { nativeModule } from "nsh";

interface WhoseApi {
  now(): string;
  later(): Promise<string>;
}
const { Whose } = nativeModule<{ Whose: WhoseApi }>("test/context");

function App() {
  const text = signal("waiting");
  setTimeout(async () => text.set(`${Whose.now()} ${await Whose.later()}`), 0);
  return (
    <View style={{ flex: 1 }}>
      <Input autoFocus value={text()} onChange={({ value }) => text.set(value)} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

/// Two surfaces side by side in one window.
struct Pair {
    a: Entity<AppView>,
    b: Entity<AppView>,
}

impl gpui::Render for Pair {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        use gpui::ParentElement as _;
        div()
            .size_full()
            .flex()
            .child(div().flex_1().child(self.a.clone()))
            .child(div().flex_1().child(self.b.clone()))
    }
}

fn each_view_s_functions_read_the_context_its_options_gave_its_engine(cx: &mut TestAppContext) {
    let mut context = nsh_core::element::Registry::ui_set("test/context");
    context.module::<Whose>();
    crate::set_ui_sets(vec![nsh_base::registry(), context]);
    let code = nsh_bundler::compile_module("App.tsx", WHOSE, &[]).unwrap();
    use gpui::AppContext as _;
    let (pair, cx) = cx.add_window_view(|window, cx| {
        let view = |desk: &'static str, window: &mut Window, cx: &mut gpui::Context<Pair>| {
            let options = AppOptions {
                context: nsh_engine::EngineContext::default().with(Desk(desk)),
                ..AppOptions::default()
            };
            cx.new(|cx| AppView::new(options, window, cx))
        };
        Pair {
            a: view("A", window, cx),
            b: view("B", window, cx),
        }
    });
    let (a, b) = pair.read_with(cx, |pair, _| (pair.a.clone(), pair.b.clone()));
    for view in [&a, &b] {
        view.update(cx, |view, _| {
            view.load(App::Js(Bundle::source(code.clone())))
        });
    }
    wait_for(&a, cx, "A A");
    wait_for(&b, cx, "B B");
}

/// A Rust view (`test/keys`) that binds a keystroke of its own.
mod rust_keys {
    use nsh_core::{element::Registry, prelude::*};

    /// The set the app imports `Shortcut` from.
    pub(super) fn registry() -> Registry {
        let mut registry = Registry::ui_set("test/keys");
        registry.register::<Shortcut>();
        registry
    }

    #[derive(Debug, Clone, PartialEq, Props)]
    pub(super) struct ShortcutProps {
        /// The keystroke, as `on_key` takes it.
        pub(super) keys: Prop<SharedString>,
    }

    #[derive(Debug, Clone, PartialEq, Event)]
    pub(super) enum ShortcutEvent {
        /// The keystroke was pressed.
        Fired,
    }

    /// Binds `keys` (in text fields too) and reports each press.
    #[component]
    pub(super) fn Shortcut(props: ShortcutProps, cx: Scope<ShortcutEvent>) -> impl IntoElement {
        let options = KeyOptions {
            when: nsh_core::keymap::When::Always,
            ..KeyOptions::default()
        };
        on_key_with(&props.keys.get(), options, move |_| {
            cx.emit(ShortcutEvent::Fired)
        });
        view()
    }
}

/// A field logging Escape (JS's binding), Shift+Enter (a Rust view's) and
/// Mod+T (JS's, which shows and hides the view). `view_first` shows the
/// view from the start and binds Escape once it mounted. Otherwise Escape
/// is bound before any Rust view exists.
fn js_and_rust_view(view_first: bool) -> String {
    format!(
        r#"
import {{ mount, signal, View }} from "nsh";
import {{ Input }} from "nsh/base";
import {{ keymap }} from "nsh";
import {{ Shortcut }} from "test/keys";

const VIEW_FIRST = {view_first};

function App() {{
  const fired = signal("fired");
  const shown = signal(VIEW_FIRST);
  const fire = (name: string) => fired.set(`${{fired()}} ${{name}}`);
  const escape = () => keymap.bind("escape", () => fire("escape"), {{ when: "always" }});
  keymap.bind("mod+t", () => {{ shown.set(!shown()); fire(shown() ? "shown" : "hidden"); }}, {{ when: "always" }});
  if (VIEW_FIRST) setTimeout(() => {{ escape(); fire("bound"); }}, 0); else escape();
  return (
    <View style={{{{ flex: 1 }}}}>
      <Input autoFocus value={{fired()}} style={{{{ height: 30 }}}} />
      <If when={{shown()}}>
        <Shortcut keys="shift+enter" onFired={{() => fire("shift")}} style={{{{ height: 10 }}}} />
      </If>
    </View>
  );
}}
mount(App);
"#
    )
}

fn js_and_rust_view_shortcuts_fire_together_in_either_order(cx: &mut TestAppContext) {
    crate::set_ui_sets(vec![nsh_base::registry(), rust_keys::registry()]);
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let sets = [rust_keys::registry().into()];

    let code = nsh_bundler::compile_module("App.tsx", &js_and_rust_view(false), &sets).unwrap();
    let (view, window) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(window, |view, _| view.load(App::Js(Bundle::source(code))));
    window.update(|w, cx| w.focus(&view.focus_handle(cx), cx));
    wait_for(&view, window, "fired");
    window.simulate_keystrokes(&format!("{primary}-t"));
    wait_for(&view, window, "fired shown");
    window.simulate_keystrokes("escape shift-enter");
    wait_for(&view, window, "fired shown escape shift");
    // The view goes, and only its binding with it.
    window.simulate_keystrokes(&format!("{primary}-t"));
    wait_for(&view, window, "fired shown escape shift hidden");
    window.simulate_keystrokes("escape");
    wait_for(&view, window, "fired shown escape shift hidden escape");

    let code = nsh_bundler::compile_module("App.tsx", &js_and_rust_view(true), &sets).unwrap();
    let (view, window) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(window, |view, _| view.load(App::Js(Bundle::source(code))));
    window.update(|w, cx| w.focus(&view.focus_handle(cx), cx));
    wait_for(&view, window, "fired bound");
    window.simulate_keystrokes("escape shift-enter");
    wait_for(&view, window, "fired bound escape shift");
    window.simulate_keystrokes(&format!("{primary}-t"));
    wait_for(&view, window, "fired bound escape shift hidden");
    window.simulate_keystrokes("escape");
    wait_for(&view, window, "fired bound escape shift hidden escape");
}

/// A pad inside 8 points of padding. A press writes where it was in the
/// pad and in the window.
mod press_point {
    use nsh_base::input;
    use nsh_core::prelude::*;

    #[component]
    pub(super) fn PressPoint() -> impl IntoElement {
        let report = signal(String::from("none"));
        view()
            .p(px(8.0))
            .child(
                view()
                    .w(px(80.0))
                    .h(px(40.0))
                    .on(EventKind::Press, move |p: &EventPayload| {
                        let EventPayload::Press(e) = p else {
                            report.set("no position".into());
                            return;
                        };
                        report.set(format!(
                            "at {:.0} {:.0} in {:.0} {:.0}",
                            e.position.x.0,
                            e.position.y.0,
                            e.page_position.x.0,
                            e.page_position.y.0
                        ));
                    }),
            )
            .child(
                input()
                    .auto_focus(true)
                    .value(move || report.get())
                    .h(px(30.0)),
            )
    }
}

fn presses_carry_where_the_pointer_was(cx: &mut TestAppContext) {
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| {
        view.load(App::Rust(Root::of::<press_point::PressPoint>()))
    });
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "none");
    cx.simulate_click(
        gpui::point(gpui::px(18.0), gpui::px(20.0)),
        gpui::Modifiers::default(),
    );
    wait_for(&view, cx, "at 10 12 in 18 20");
}

/// Runs the window until the focused field's text satisfies `done`, and
/// returns it.
fn wait_until(
    view: &Entity<AppView>,
    cx: &mut VisualTestContext,
    done: impl Fn(&str) -> bool,
) -> String {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        let now = field_text(view, cx).unwrap_or_default();
        if done(&now) {
            return now;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the field reads {now:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A view the window's root holds, as an embedding app's frame does.
struct Framed(Entity<AppView>);

impl gpui::Render for Framed {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        use gpui::ParentElement as _;
        div().size_full().child(self.0.clone())
    }
}

/// Opens a window whose root holds an [`AppView`] that owns the window,
/// running `code`, with the keyboard in the view.
fn owned_view(
    cx: &mut TestAppContext,
    code: String,
    setup: impl FnOnce(&AppView),
) -> (Entity<AppView>, &mut VisualTestContext) {
    use gpui::AppContext as _;
    let (framed, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            let view = AppView::new(AppOptions::default(), window, cx);
            view.own_window(window, cx);
            view
        });
        Framed(view)
    });
    let view = framed.read_with(cx, |framed, _| framed.0.clone());
    view.update(cx, |view, _| {
        setup(view);
        view.load(App::Js(Bundle::source(code)));
    });
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    (view, cx)
}

/// Runs the window until `done` holds.
fn wait_until_window(
    cx: &mut VisualTestContext,
    what: &str,
    done: impl Fn(&mut VisualTestContext) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "never: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The height available below the title bar, before the menu bar.
fn height_below_title_bar(cx: &mut VisualTestContext) -> f32 {
    let full = cx.update(|window, _| f32::from(window.viewport_size().height));
    // Windows draws a 32-point caption inside the viewport; Linux's
    // window manager draws its decorations outside it.
    full - if cfg!(windows) { 32.0 } else { 0.0 }
}

/// A field that reads the app's height (`h <points>`, the window less the
/// chrome around the app) and ` new` once New was chosen, under a title
/// and an application menu: File with New, Quit (the `quit` role, with a
/// shortcut of its own) and Close (the `close` role).
const OWNED_WINDOW: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";
import { Menu, Window } from "nsh/desktop";

function App() {
  const height = signal(-1);
  const chosen = signal("");
  Window.setTitle("Owned");
  Menu.setApplicationMenu([
    {
      label: "File",
      submenu: [
        { label: "New", shortcut: "mod+shift+n", onSelect: () => chosen.set(" new") },
        { label: "Quit", role: "quit", shortcut: "mod+shift+q" },
        { label: "Close", role: "close", shortcut: "mod+shift+w" },
      ],
    },
  ]);
  return (
    <View style={{ flex: 1 }} onLayout={({ bounds }) => height.set(Math.round(bounds.size.height))}>
      <Input autoFocus value={`h ${height()}${chosen()}`} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

/// The app's height the field reads.
fn app_height(view: &Entity<AppView>, cx: &mut VisualTestContext) -> f32 {
    let text = wait_until(view, cx, |t| t.starts_with("h ") && !t.starts_with("h -1"));
    text.split(' ').nth(1).and_then(|h| h.parse().ok()).unwrap()
}

fn an_embedded_view_that_owns_its_window_sets_its_title_and_menu(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", OWNED_WINDOW, &[]).unwrap();
    let (view, cx) = owned_view(cx, code, |_| {});
    wait_until_window(cx, "the app's title on the window", |cx| {
        cx.window_title().as_deref() == Some("Owned")
    });
    if cfg!(target_os = "linux") {
        // The menu bar is drawn above the app, 28 points tall.
        let window = height_below_title_bar(cx);
        wait_until(&view, cx, |t| t == format!("h {}", window - 28.0));
    }
}

fn quit_and_close_requests_go_to_the_handler_instead_of_acting(cx: &mut TestAppContext) {
    use crate::CloseRequest::{Close, Quit};
    let code = nsh_bundler::compile_module("App.tsx", OWNED_WINDOW, &[]).unwrap();
    let asked = Rc::new(std::cell::RefCell::new(Vec::new()));
    let log = asked.clone();
    let (view, cx) = owned_view(cx, code, move |view| {
        view.on_close_request(move |request, _, _| log.borrow_mut().push(request))
    });
    app_height(&view, cx);
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let mut expected = Vec::new();
    for (keys, request) in [
        // The reserved quit key, Ctrl+W, and the menu's roles by their
        // shortcuts.
        (format!("{primary}-q"), Quit),
        (format!("{primary}-w"), Close),
        (format!("shift-{primary}-q"), Quit),
        (format!("shift-{primary}-w"), Close),
    ] {
        cx.simulate_keystrokes(&keys);
        expected.push(request);
        wait_until_window(cx, &format!("{keys} asks {request:?}"), |_| {
            *asked.borrow() == expected
        });
    }
    // The window's close button: the window stays.
    assert!(!cx.simulate_close(), "the close button closed the window");
    expected.push(Close);
    wait_until_window(cx, "the close button asks Close", |_| {
        *asked.borrow() == expected
    });
    // Still open, still running: the menu's other items work.
    let height = app_height(&view, cx);
    cx.simulate_keystrokes(&format!("shift-{primary}-n"));
    wait_for(&view, cx, &format!("h {height} new"));
}

/// The app's height under an auto-hidden menu bar (and ` new` once File
/// > New was chosen).
const AUTO_HIDDEN_BAR: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";
import { Menu, Window } from "nsh/desktop";

function App() {
  const height = signal(-1);
  const chosen = signal("");
  Window.setMenuBar("autoHide");
  Menu.setApplicationMenu([
    { label: "File", submenu: [{ label: "New", onSelect: () => chosen.set(" new") }] },
  ]);
  return (
    <View style={{ flex: 1 }} onLayout={({ bounds }) => height.set(Math.round(bounds.size.height))}>
      <Input autoFocus value={`h ${height()}${chosen()}`} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

fn an_auto_hidden_menu_bar_shows_on_alt_alone_and_while_a_menu_is_open(cx: &mut TestAppContext) {
    // macOS's menu bar is the system's.
    if !cfg!(target_os = "linux") && !cfg!(windows) {
        return;
    }
    let code = nsh_bundler::compile_module("App.tsx", AUTO_HIDDEN_BAR, &[]).unwrap();
    let (view, cx) = owned_view(cx, code, |_| {});
    let full = height_below_title_bar(cx);
    let (hidden, shown) = (format!("h {full}"), format!("h {}", full - 28.0));
    wait_for(&view, cx, &hidden);
    let alt_alone = |cx: &mut VisualTestContext| {
        cx.simulate_modifiers_change(gpui::Modifiers::alt());
        cx.simulate_modifiers_change(gpui::Modifiers::none());
    };
    alt_alone(cx);
    wait_for(&view, cx, &shown);
    alt_alone(cx);
    wait_for(&view, cx, &hidden);
    // Alt with a key or another modifier is a shortcut: Alt + F opens File
    // (the bar shows while it is open), and the bar hides again as it
    // closes.
    cx.simulate_modifiers_change(gpui::Modifiers::alt());
    cx.simulate_keystrokes("alt-f");
    cx.simulate_modifiers_change(gpui::Modifiers::none());
    wait_for(&view, cx, &shown);
    cx.simulate_keystrokes("escape");
    wait_for(&view, cx, &hidden);
    cx.simulate_modifiers_change(gpui::Modifiers::alt());
    cx.simulate_modifiers_change(gpui::Modifiers {
        alt: true,
        shift: true,
        ..Default::default()
    });
    cx.simulate_modifiers_change(gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        field_text(&view, cx),
        Some(hidden.clone()),
        "Alt + Shift showed the bar"
    );
    // F10 opens the first menu; Return chooses its first item, and the bar
    // hides again.
    cx.simulate_keystrokes("f10");
    wait_for(&view, cx, &shown);
    cx.simulate_keystrokes("enter");
    wait_for(&view, cx, &format!("{hidden} new"));
}

/// Two images from two asset directories: each reports its size, or why
/// it did not load.
const TWO_ASSET_ROOTS: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";
import { Image } from "nsh";

function App() {
  const text = signal("waiting");
  const seen: string[] = [];
  const report = (name: string, what: string) => {
    seen.push(`${name} ${what}`);
    if (seen.length === 2) text.set(seen.sort().join("|"));
  };
  return (
    <View style={{ flex: 1 }}>
      <Image
        src="asset:/app.png"
        onLoad={({ size }) => report("app", `${size.width}x${size.height}`)}
        onError={(e) => report("app", e.message)}
        style={{ width: 10, height: 10 }}
      />
      <Image
        src="asset:/bytes/cover.png"
        onLoad={({ size }) => report("cover", `${size.width}x${size.height}`)}
        onError={(e) => report("cover", e.message)}
        style={{ width: 10, height: 10 }}
      />
      <Input autoFocus value={text()} onChange={({ value }) => text.set(value)} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

fn asset_uris_resolve_in_the_first_directory_that_has_the_file(cx: &mut TestAppContext) {
    let root = std::env::temp_dir().join(format!("nsh-asset-roots-{}", std::process::id()));
    let (app, archive) = (root.join("app"), root.join("archive"));
    std::fs::create_dir_all(archive.join("bytes")).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    let png = |path: std::path::PathBuf, w: u32, h: u32| {
        image::RgbaImage::new(w, h).save(path).unwrap();
    };
    png(app.join("app.png"), 2, 3);
    png(archive.join("bytes/cover.png"), 4, 5);
    // The archive's own `app.png` is not the one shown: the app's comes
    // first.
    png(archive.join("app.png"), 7, 7);
    let code = nsh_bundler::compile_module("App.tsx", TWO_ASSET_ROOTS, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| {
        view.set_asset_base(Some(crate::AssetBase::Directories(vec![app, archive])));
        view.load(App::Js(Bundle::source(code)));
    });
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    let report = wait_until(&view, cx, |t| t.contains('|'));
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(report, "app 2x3|cover 4x5");
}

fn releasing_shift_before_alt_keeps_the_auto_hidden_menu_bar_hidden(cx: &mut TestAppContext) {
    if !cfg!(target_os = "linux") && !cfg!(windows) {
        return;
    }
    let code = nsh_bundler::compile_module("App.tsx", AUTO_HIDDEN_BAR, &[]).unwrap();
    let (view, cx) = owned_view(cx, code, |_| {});
    let full = height_below_title_bar(cx);
    wait_for(&view, cx, &format!("h {full}"));
    for shift_first in [false, true] {
        cx.simulate_modifiers_change(if shift_first {
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            }
        } else {
            gpui::Modifiers::alt()
        });
        cx.simulate_modifiers_change(gpui::Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        });
        cx.simulate_modifiers_change(gpui::Modifiers::alt());
        cx.simulate_modifiers_change(gpui::Modifiers::none());
        cx.run_until_parked();
        let height = view.read_with(cx, |view, _| view.surface().size().1);
        assert_eq!(
            height, full,
            "Alt+Shift showed the bar after Shift was released first (shift_first={shift_first})"
        );
    }
}

const MENU_OVER_FIELD: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";
import { Menu } from "nsh/desktop";

function App() {
  const text = signal("abcd");
  const chose = (name: string) => () => text.set(`${text()} ${name}`);
  Menu.setApplicationMenu([
    {
      label: "Edit",
      submenu: [
        { label: "Revert", shortcut: "mod+z", when: "auto", onSelect: chose("revert") },
        { label: "Redo app", shortcut: "mod+y", when: "auto", onSelect: chose("app-redo") },
        { label: "Remove", shortcut: "delete", when: "auto", onSelect: chose("remove") },
        { label: "Send", shortcut: "mod+enter", when: "auto", onSelect: chose("send") },
        { label: "Purge", shortcut: "shift+delete", onSelect: chose("purge") },
      ],
    },
  ]);
  return (
    <View style={{ flex: 1 }}>
      <Input
        autoFocus
        submitBehavior="submit"
        value={text()}
        onChange={({ value }) => text.set(value)}
        style={{ height: 30 }}
      />
    </View>
  );
}
mount(App);
"#;

fn menu_shortcuts_leave_a_focused_field_the_keys_it_uses(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", MENU_OVER_FIELD, &[]).unwrap();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = AppView::new(AppOptions::default(), window, cx);
        let title = window.window_title();
        crate::chrome::set_standalone(view.surface(), &title, window.window_handle());
        crate::system::own_window(view.surface());
        view
    });
    view.update(cx, |view, _| view.load(App::Js(Bundle::source(code))));
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    wait_for(&view, cx, "abcd");
    cx.simulate_keystrokes("end");
    cx.simulate_input("e");
    wait_for(&view, cx, "abcde");
    cx.simulate_keystrokes(&format!("{primary}-z"));
    wait_for(&view, cx, "abcd");
    cx.simulate_keystrokes(&format!("{primary}-y"));
    wait_for(&view, cx, "abcde");
    cx.simulate_keystrokes(&format!("{primary}-z"));
    wait_for(&view, cx, "abcd");
    cx.simulate_keystrokes("home delete");
    wait_for(&view, cx, "bcd");
    cx.simulate_keystrokes(&format!("{primary}-enter"));
    cx.run_until_parked();
    cx.simulate_keystrokes("shift-delete");
    wait_for(&view, cx, "bcd purge");
    cx.simulate_keystrokes("escape");
    wait_until_blurred(&view, cx);
    cx.simulate_keystrokes(&format!("{primary}-z {primary}-y delete {primary}-enter"));
    cx.run_until_parked();
    let (x, y) = view.read_with(cx, |view, _| view.surface().origin.get());
    cx.simulate_click(
        gpui::point(gpui::px(x + 20.0), gpui::px(y + 15.0)),
        gpui::Modifiers::default(),
    );
    wait_for(&view, cx, "bcd purge revert app-redo remove send");
}

fn wait_until_blurred(view: &Entity<AppView>, cx: &mut VisualTestContext) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if field_text(view, cx).is_none() {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the field still has focus"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A pressable strip over a focused field that shows what each press
/// carried: the field reads the modifier keys and click count of the last
/// press.
const PRESS_MODIFIERS: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";

function App() {
  const text = signal("none");
  return (
    <View style={{ flex: 1 }}>
      <View
        focusable={false}
        onPress={(e) =>
          text.set(
            e
              ? `ctrl ${e.modifiers.control} shift ${e.modifiers.shift} alt ${e.modifiers.alt} meta ${e.modifiers.platform} clicks ${e.clickCount}`
              : "no event",
          )
        }
        style={{ height: 40 }}
      />
      <Input autoFocus value={text()} onChange={({ value }) => text.set(value)} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

fn presses_carry_the_modifier_keys_and_click_count(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", PRESS_MODIFIERS, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| view.load(App::Js(Bundle::source(code))));
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "none");
    let strip = gpui::point(gpui::px(20.0), gpui::px(20.0));
    cx.simulate_click(strip, gpui::Modifiers::control());
    wait_for(
        &view,
        cx,
        "ctrl true shift false alt false meta false clicks 1",
    );
    // A double click is two presses, the second counted as such.
    click(cx, strip, 1);
    click(cx, strip, 2);
    wait_for(
        &view,
        cx,
        "ctrl false shift false alt false meta false clicks 2",
    );
}

/// A pad 100 × 40 at (10, 10) over a focused field that shows the pad's
/// last `onMouseEnter`, `onMouseMove` and `onMouseLeave`: where the mouse
/// was in the pad and the window, the button held and Shift.
const MOUSE_EVENTS: &str = r#"
import { mount, signal, View } from "nsh";
import { Input } from "nsh/base";

const show = (name, e) =>
  `${name} ${Math.round(e.position.x)} ${Math.round(e.position.y)} in ${Math.round(e.pagePosition.x)} ${Math.round(e.pagePosition.y)} ${e.pressedButton} ${e.modifiers.shift}`;

function App() {
  const enter = signal("-");
  const move = signal("-");
  const leave = signal("-");
  return (
    <View style={{ flex: 1 }}>
      <View
        onMouseEnter={(e) => enter.set(show("enter", e))}
        onMouseMove={(e) => move.set(show("move", e))}
        onMouseLeave={(e) => leave.set(show("leave", e))}
        style={{ marginLeft: 10, marginTop: 10, width: 100, height: 40 }}
      />
      <Input autoFocus value={`${enter()} | ${move()} | ${leave()}`} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

fn mouse_events_carry_the_position_button_and_modifiers(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", MOUSE_EVENTS, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| view.load(App::Js(Bundle::source(code))));
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "- | - | -");
    let none = gpui::Modifiers::default();
    // The move that brings the mouse in enters the pad and moves over it.
    cx.simulate_mouse_move(gpui::point(gpui::px(30.0), gpui::px(25.0)), None, none);
    wait_for(
        &view,
        cx,
        "enter 20 15 in 30 25 null false | move 20 15 in 30 25 null false | -",
    );
    // Moves with a button down are moves too, with the button.
    cx.simulate_mouse_move(
        gpui::point(gpui::px(40.0), gpui::px(30.0)),
        gpui::MouseButton::Left,
        none,
    );
    wait_for(
        &view,
        cx,
        "enter 20 15 in 30 25 null false | move 30 20 in 40 30 left false | -",
    );
    cx.simulate_mouse_move(gpui::point(gpui::px(50.0), gpui::px(30.0)), None, none);
    wait_for(
        &view,
        cx,
        "enter 20 15 in 30 25 null false | move 40 20 in 50 30 null false | -",
    );
    // A modifier key changing while the mouse rests is a move where it is.
    cx.simulate_modifiers_change(gpui::Modifiers::shift());
    wait_for(
        &view,
        cx,
        "enter 20 15 in 30 25 null false | move 40 20 in 50 30 null true | -",
    );
    cx.simulate_modifiers_change(none);
    cx.simulate_mouse_move(gpui::point(gpui::px(120.0), gpui::px(30.0)), None, none);
    wait_for(
        &view,
        cx,
        "enter 20 15 in 30 25 null false | move 40 20 in 50 30 null false | leave 110 20 in 120 30 null false",
    );
}

/// A left click at `position` that is click `count` of a series.
fn click(cx: &mut VisualTestContext, position: gpui::Point<gpui::Pixels>, count: usize) {
    let (button, modifiers) = (gpui::MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_event(gpui::MouseDownEvent {
        position,
        button,
        modifiers,
        click_count: count,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        position,
        button,
        modifiers,
        click_count: count,
    });
}

use std::borrow::Cow;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::{FileChange, FileChangeKind, ItemContent, ItemStatus, TurnStatus};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Task, Window,
    WindowBounds, WindowOptions, div, px, size,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_core::notice::{CallbackEvent, OrchestrateCallback};
use tcode_core::session::{EntryContent, TimelineEntry};
use tcode_ui::icon::IconName;
use tcode_ui::sizing::Sizable as _;
use tcode_ui::theme::{self, ActiveTheme as _, ThemeMode};
use tcode_ui::widgets::choice::{Choice, ChoiceGroup};
use tcode_ui::widgets::tabs::{TabBar, TabItem};
use tcode_ui::widgets::{Button, ButtonVariants as _, Switch};
use tcode_ui::{assets, gallery_support, markdown::MarkdownState};

const ASSISTANT_MARKDOWN: &str = r#"Here is a compact result with **real markdown rendering** and a link to [the guide](https://example.com).

```rust
fn soft_wrapping_example(request: &str) -> Result<String, GalleryError> { process_request_without_truncating_the_important_context(request) }
```

For \(H(\theta)=\nabla^2 L(\theta)\), a smooth curvature measure is

\[
F_\tau(H)=\tau\log\operatorname{tr}\exp(H/\tau),\qquad
\rho_\tau(H)=\frac{\exp(H/\tau)}{\operatorname{tr}\exp(H/\tau)}.
\]

| State | Result | Duration |
| --- | ---: | ---: |
| Parsed | 24 files | 1.2s |
| Verified | 238 tests | 4.8s |

```mermaid
flowchart LR
  Client[Client] -->|SessionCommand| Host[Host]
  Host -->|AgentEvent| Client
  Host --> Provider{Provider}
  Provider --> Claude
  Provider --> Codex
```
"#;

struct Gallery {
    markdown: Entity<MarkdownState>,
    started_at: u64,
    tabs: TabsState,
    options: Options,
    _tick: Task<()>,
}

/// Launch flags: `--dark` starts in the dark theme, `--section N` shows only
/// section N, `--width W` opens the window W points wide, and `--tab N`
/// presses Tab N times once the first frame is drawn, to show focus rings.
#[derive(Clone, Copy, Default)]
struct Options {
    dark: bool,
    section: Option<usize>,
    width: Option<f32>,
    tab: usize,
}

impl Options {
    fn parse() -> Self {
        let mut options = Self::default();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dark" => options.dark = true,
                "--section" => options.section = args.next().and_then(|n| n.parse().ok()),
                "--width" => options.width = args.next().and_then(|w| w.parse().ok()),
                "--tab" => options.tab = args.next().and_then(|n| n.parse().ok()).unwrap_or(0),
                _ => {}
            }
        }
        options
    }
}

/// The selected item of each bar in the tabs section, by bar.
#[derive(Default)]
struct TabsState {
    selected: [usize; 9],
}

impl Gallery {
    fn new(options: Options, cx: &mut Context<Self>) -> Self {
        let markdown = cx.new(|cx| MarkdownState::new(ASSISTANT_MARKDOWN, cx));
        let started_at = now_millis().saturating_sub(7_300);
        let tick = cx.spawn(async move |this, cx| {
            loop {
                smol::Timer::after(Duration::from_millis(100)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            markdown,
            started_at,
            tabs: TabsState {
                selected: [0, 0, 0, 1, 1, 0, 0, 0, 2],
            },
            options,
            _tick: tick,
        }
    }
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        let command_collapsed = entry(
            "gallery-s03-activity-command-collapsed",
            ItemContent::CommandExecution {
                command: "cargo clippy --workspace --all-targets -- -D warnings".into(),
                output: (1..=26)
                    .map(|line| format!("checking crate {line:02} ... ok"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            },
        );
        let mut command_expanded = command_collapsed.clone();
        command_expanded.id = "gallery-s03-activity-command-expanded".into();
        if let EntryContent::Item(ItemContent::CommandExecution { command, .. }) =
            &mut command_expanded.content
        {
            *command = "/bin/zsh -lc \"git switch main\ngit merge --no-ff fix/raw-upload-streaming -m 'fix: 合入原始数据流式上传与自动续票'\ngit push origin main\"".into();
        }
        let tool = entry(
            "gallery-s03-activity-tool-call",
            ItemContent::ToolCall {
                presentation: None,
                image_reads: Vec::new(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "crates/ui/src/chat/components/activity.rs"}),
                output: Some("Loaded 341 lines".into()),
                status: ItemStatus::Completed,
            },
        );
        let thinking = entry(
            "gallery-s03-activity-settled-thinking",
            ItemContent::Reasoning {
                text: "Compared the component API with the gallery fixtures and verified that the catalog only exercises public rendering inputs.".into(),
            },
        );
        let subagents = [
            subagent_entry("gallery-s05-subagent-running", ItemStatus::InProgress, None),
            subagent_entry(
                "gallery-s05-subagent-completed",
                ItemStatus::Completed,
                Some("Mapped all component states"),
            ),
            subagent_entry(
                "gallery-s05-subagent-failed",
                ItemStatus::Failed,
                Some("Fixture validation failed"),
            ),
        ];
        let two_files = file_changes(2);
        let five_files = file_changes(5);
        let callback = OrchestrateCallback {
            child_id: "child-gallery".into(),
            title: "Audit the extracted chat components".into(),
            event: CallbackEvent::Ended("reported".into()),
            body: "Checked the activity, work-log, and disclosure render paths.\nAll fixtures are isolated from live session state.".into(),
        };

        let sections = vec![
            section(
                1,
                "Working indicator",
                state_row(vec![state_card(
                    "ANIMATING",
                    gallery_support::working_indicator(
                        "gallery-s01-working-animating",
                        self.started_at,
                        cx,
                    ),
                    cx,
                )]),
                cx,
            ),
            section(
                2,
                "User bubble",
                state_stack(vec![
                    state_card_full(
                        "SHORT",
                        gallery_support::user_bubble(
                            "gallery-s02-user-short",
                            "Ship the gallery.",
                            false,
                            cwd,
                            window,
                            cx,
                        ),
                        cx,
                    ),
                    state_card_full(
                        "LONG WRAPPING",
                        gallery_support::user_bubble(
                            "gallery-s02-user-long-wrapping",
                            "Please verify every extracted component against the redesign spec and keep this intentionally long message wrapping naturally inside its bubble.",
                            false,
                            cwd,
                            window,
                            cx,
                        ),
                        cx,
                    ),
                    state_card_full(
                        "PENDING STEER",
                        gallery_support::user_bubble(
                            "gallery-s02-user-pending-steer",
                            "Also include the failure states.",
                            true,
                            cwd,
                            window,
                            cx,
                        ),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                3,
                "Activity rows",
                state_stack(vec![
                    state_card_full(
                        "COMMAND · COLLAPSED",
                        gallery_support::activity(&command_collapsed, false, cx),
                        cx,
                    ),
                    state_card_full(
                        "COMMAND · EXPANDED + OUTPUT TAIL",
                        gallery_support::activity(&command_expanded, true, cx),
                        cx,
                    ),
                    state_card_full("TOOL CALL", gallery_support::activity(&tool, false, cx), cx),
                    state_card_full(
                        "SETTLED THINKING",
                        gallery_support::activity(&thinking, false, cx),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                4,
                "Work-log capsule",
                state_row(vec![
                    state_card(
                        "SETTLED · COUNTS + DURATION",
                        gallery_support::work_log(
                            "gallery-s04-worklog-settled",
                            401,
                            "2 tools · 3 edits · 1 command",
                            "18.6s",
                            TurnStatus::Completed,
                            cx,
                        ),
                        cx,
                    ),
                    state_card(
                        "TURN FAILED",
                        gallery_support::work_log(
                            "gallery-s04-worklog-failed",
                            402,
                            "1 tool · 1 command",
                            "4.2s",
                            TurnStatus::Failed,
                            cx,
                        ),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                5,
                "Subagent capsule",
                state_row(
                    subagents
                        .iter()
                        .zip(["RUNNING", "COMPLETED", "FAILED"])
                        .map(|(entry, label)| {
                            state_card(label, gallery_support::subagent(entry, cx), cx)
                        })
                        .collect(),
                ),
                cx,
            ),
            section(
                6,
                "Changed-files chips",
                state_row(vec![
                    state_card(
                        "2 FILES · FLAT",
                        gallery_support::changed_files(601, cwd, &two_files, cx),
                        cx,
                    ),
                    state_card(
                        "5 FILES · +2 MORE",
                        gallery_support::changed_files(602, cwd, &five_files, cx),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                7,
                "Assistant block",
                state_row(vec![state_card(
                    "PROSE · SOFT-WRAPPING CODE · TABLE",
                    gallery_support::assistant(
                        "gallery-s07-assistant-markdown",
                        self.markdown.clone(),
                        cwd,
                        cx,
                    ),
                    cx,
                )]),
                cx,
            ),
            section(
                8,
                "Error card",
                state_row(vec![state_card(
                    "MULTI-LINE ERROR",
                    gallery_support::error_card(
                        "gallery-s08-error-multiline",
                        "Provider request failed after three attempts.\nThe upstream connection closed before the response body completed.\nRetry the turn or select another model.",
                        cx,
                    ),
                    cx,
                )]),
                cx,
            ),
            section(
                9,
                "Dividers",
                state_row(vec![
                    state_card(
                        "RELAY",
                        gallery_support::relay_divider("gallery-s09-divider-relay", cx),
                        cx,
                    ),
                    state_card(
                        "MODEL CHANGE",
                        gallery_support::model_change_divider(
                            "gallery-s09-divider-model-change",
                            cx,
                        ),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                10,
                "Disclosure",
                state_row(vec![
                    state_card(
                        "COLLAPSED CALLBACK",
                        gallery_support::callback(
                            "gallery-s10-disclosure-collapsed",
                            &callback,
                            false,
                            cx,
                        ),
                        cx,
                    ),
                    state_card(
                        "EXPANDED CALLBACK",
                        gallery_support::callback(
                            "gallery-s10-disclosure-expanded",
                            &callback,
                            true,
                            cx,
                        ),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(11, "Tabs and choices", self.tabs_section(cx), cx),
        ];
        let only = self.options.section;
        let sections = sections
            .into_iter()
            .enumerate()
            .filter(|(ix, _)| only.is_none_or(|section| section == ix + 1))
            .map(|(_, section)| section);

        div()
            .id("gallery-scroll")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex().w_full().justify_center().child(
                    v_flex()
                        .w(px(720.))
                        .max_w_full()
                        .gap(px(56.))
                        .px_5()
                        .pt(px(64.))
                        .pb(px(96.))
                        .child(
                            v_flex()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(28.))
                                                .font_semibold()
                                                .child("Chat component gallery"),
                                        )
                                        .child(
                                            Switch::new("gallery-dark")
                                                .label("Dark")
                                                .checked(cx.theme().mode.is_dark())
                                                .on_click(|dark, window, cx| {
                                                    theme::change_mode(
                                                        if *dark {
                                                            ThemeMode::Dark
                                                        } else {
                                                            ThemeMode::Light
                                                        },
                                                        Some(window),
                                                        cx,
                                                    )
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Real extracted GPUI components rendered with deterministic fixture data."),
                                ),
                        )
                        .children(sections),
                ),
            )
    }
}

impl Gallery {
    fn tabs_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.tabs.selected;
        let view = cx.entity();
        let choose = move |bar: usize, ix: usize| {
            let view = view.clone();
            move |_: &mut Window, cx: &mut App| {
                view.update(cx, |this, cx| {
                    this.tabs.selected[bar] = ix;
                    cx.notify();
                })
            }
        };
        let pick = |bar: usize, ix: usize, _: &mut Context<Self>| {
            let choose = choose(bar, ix);
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| choose(window, cx)
        };
        let bar = |bar: usize,
                   id: &'static str,
                   items: Vec<(&'static str, Option<IconName>, Option<usize>)>,
                   cx: &mut Context<Self>| {
            items
                .into_iter()
                .enumerate()
                .map(|(ix, (label, icon, count))| {
                    let item = TabItem::new((id, ix), label)
                        .selected(selected[bar] == ix)
                        .on_click(pick(bar, ix, cx));
                    let item = match icon {
                        Some(icon) => item.icon(icon),
                        None => item,
                    };
                    match count {
                        Some(count) => item.count(count),
                        None => item,
                    }
                })
                .collect::<Vec<_>>()
        };
        let panel = vec![
            ("Diff", Some(IconName::File), None),
            ("Plan", Some(IconName::Map), None),
            ("Agents", Some(IconName::Bot), Some(3)),
            ("Pull requests", Some(IconName::GitPullRequest), Some(12)),
        ];
        let pr_views = vec![
            ("Files", None, Some(24)),
            ("Commits", None, Some(3)),
            ("Conversation", None, Some(7)),
        ];
        let settings = vec![
            ("General", Some(IconName::Settings), None),
            ("Providers", Some(IconName::Bot), None),
            ("Source control", Some(IconName::GitBranch), None),
            ("Plugins", Some(IconName::Puzzle), Some(4)),
            ("Remote access", Some(IconName::Globe), None),
        ];
        let long = vec![
            ("Repository overview and activity", None, None),
            ("Continuous integration pipelines", None, None),
            ("Deployment environments", None, None),
        ];
        let terminals =
            ["zsh", "cargo watch", "npm run dev"]
                .into_iter()
                .enumerate()
                .map(|(ix, label)| {
                    TabItem::new(("gallery-terminal", ix), label)
                        .selected(selected[4] == ix)
                        .on_click(pick(4, ix, cx))
                        .trailing(
                            h_flex()
                                .gap_1()
                                .children((ix == 2).then(|| {
                                    div().size(px(6.)).rounded_full().bg(cx.theme().warning)
                                }))
                                .child(
                                    Button::new(("gallery-terminal-close", ix))
                                        .ghost()
                                        .compact()
                                        .xsmall()
                                        .icon(IconName::Close)
                                        .tooltip("Close terminal"),
                                ),
                        )
                })
                .collect::<Vec<_>>();
        let merge = ["Merge commit", "Squash and merge", "Rebase and merge"]
            .into_iter()
            .enumerate()
            .map(|(ix, label)| {
                Choice::new(("gallery-merge", ix), label)
                    .selected(selected[5] == ix)
                    .recommended(ix == 1)
                    .disabled(ix == 2)
                    .on_select(choose(5, ix))
            })
            .collect::<Vec<_>>();
        let effort = ["Low", "Medium", "High", "Max"]
            .into_iter()
            .enumerate()
            .map(|(ix, label)| {
                Choice::new(("gallery-effort", ix), label)
                    .selected(selected[8] == ix)
                    .recommended(ix == 2)
                    .on_select(choose(8, ix))
            })
            .collect::<Vec<_>>();
        let label = |text: &'static str, cx: &App| {
            div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(text)
        };

        state_stack(vec![
            state_card_full(
                "UNDERLINE · ICONS + COUNT",
                TabBar::new("gallery-underline")
                    .aria_label("Panel")
                    .items(bar(0, "gallery-panel", panel.clone(), cx))
                    .into_any_element(),
                cx,
            ),
            state_card_full(
                "UNDERLINE · TRAILING CONTROLS",
                TabBar::new("gallery-terminals")
                    .aria_label("Terminals")
                    .items(terminals)
                    .into_any_element(),
                cx,
            ),
            state_card_full(
                "SEGMENTED",
                TabBar::new("gallery-segmented")
                    .segmented()
                    .aria_label("Pull request view")
                    .items(bar(1, "gallery-pr-view", pr_views.clone(), cx))
                    .into_any_element(),
                cx,
            ),
            state_row(vec![
                state_card(
                    "RAIL · TEXT",
                    TabBar::new("gallery-rail")
                        .rail()
                        .aria_label("Settings")
                        .w(px(220.))
                        .items(bar(2, "gallery-settings", settings.clone(), cx))
                        .into_any_element(),
                    cx,
                ),
                state_card(
                    "RAIL · ICON-ONLY",
                    TabBar::new("gallery-rail-icons")
                        .rail()
                        .icon_only(true)
                        .aria_label("Settings")
                        .w(px(44.))
                        .items(bar(3, "gallery-settings-icons", settings, cx))
                        .into_any_element(),
                    cx,
                ),
            ]),
            state_card_full(
                "CHOICE · RECOMMENDED + DISABLED",
                v_flex()
                    .gap(px(6.))
                    .child(label("Merge method", cx))
                    .child(
                        ChoiceGroup::new("gallery-merge")
                            .aria_label("Merge method")
                            .choices(merge),
                    )
                    .child(div().h(px(8.)))
                    .child(label("Reasoning effort", cx))
                    .child(
                        div().w(px(360.)).child(
                            ChoiceGroup::new("gallery-effort")
                                .aria_label("Reasoning effort")
                                .choices(effort),
                        ),
                    )
                    .into_any_element(),
                cx,
            ),
            state_card_full(
                "LONG LABELS · SCROLL",
                v_flex()
                    .w(px(320.))
                    .gap_3()
                    .child(
                        TabBar::new("gallery-long-underline")
                            .aria_label("Long")
                            .items(bar(6, "gallery-long", long.clone(), cx)),
                    )
                    .child(
                        TabBar::new("gallery-long-segmented")
                            .segmented()
                            .aria_label("Long")
                            .items(bar(7, "gallery-long-seg", long, cx)),
                    )
                    .into_any_element(),
                cx,
            ),
            state_card_full(
                "COMPACT · 44PT TARGETS",
                v_flex()
                    .w(px(360.))
                    .gap_3()
                    .child(
                        TabBar::new("gallery-compact-underline")
                            .compact(true)
                            .aria_label("Panel")
                            .items(bar(0, "gallery-compact-panel", panel, cx)),
                    )
                    .child(
                        TabBar::new("gallery-compact-segmented")
                            .segmented()
                            .compact(true)
                            .aria_label("Pull request view")
                            .items(bar(1, "gallery-compact-pr-view", pr_views, cx)),
                    )
                    .into_any_element(),
                cx,
            ),
        ])
    }
}

fn entry(id: &str, content: ItemContent) -> TimelineEntry {
    TimelineEntry {
        id: id.into(),
        content: EntryContent::Item(content),
        ts: None,
        turn: 0,
    }
}

fn subagent_entry(id: &str, status: ItemStatus, summary: Option<&str>) -> TimelineEntry {
    entry(
        id,
        ItemContent::Subagent {
            agent_type: "explorer".into(),
            description: "Review the extracted component fixtures".into(),
            status,
            summary: summary.map(str::to_string),
            model: None,
            effort: None,
        },
    )
}

fn file_changes(count: usize) -> Vec<FileChange> {
    let paths = [
        "crates/ui/src/chat/components/activity.rs",
        "crates/ui/src/chat/components/assistant.rs",
        "crates/ui/src/chat/components/bubble.rs",
        "crates/ui/src/chat/components/disclosure.rs",
        "crates/ui/examples/gallery.rs",
    ];
    paths[..count]
        .iter()
        .enumerate()
        .map(|(index, path)| FileChange {
            path: (*path).into(),
            kind: FileChangeKind::Modify,
            diff: Some(format!(
                "@@ -1,2 +1,{} @@\n old line\n+new line\n+fixture {}\n",
                index + 3,
                index + 1
            )),
        })
        .collect()
}

fn section(number: usize, title: &str, content: impl IntoElement, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .gap_3()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(format!("{number:02}")),
                )
                .child(div().h(px(1.)).w_5().bg(cx.theme().border))
                .child(gallery_support::section_label(title)),
        )
        .child(content)
        .into_any_element()
}

fn state_row(cards: Vec<AnyElement>) -> AnyElement {
    h_flex()
        .w_full()
        .items_stretch()
        .gap_3()
        .flex_wrap()
        .children(cards)
        .into_any_element()
}

fn state_stack(cards: Vec<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .items_start()
        .gap_3()
        .children(cards)
        .into_any_element()
}

fn state_card(label: &str, content: AnyElement, cx: &App) -> AnyElement {
    v_flex()
        .min_w(px(210.))
        .flex_1()
        .gap_3()
        .p_4()
        .rounded(px(14.))
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted.opacity(0.28))
        .child(
            div()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(gallery_support::section_label(label)),
        )
        .child(content)
        .into_any_element()
}

fn state_card_full(label: &str, content: AnyElement, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .gap_3()
        .p_4()
        .rounded(px(14.))
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted.opacity(0.28))
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(gallery_support::section_label(label)),
        )
        .child(content)
        .into_any_element()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn main() {
    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(|cx| {
            tcode_ui::markdown::init(cx);
            tcode_ui::settings::apply_locale(Some(tcode_ui::LANGUAGE_ENGLISH));

            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let fonts: Vec<Cow<'static, [u8]>> = vec![
                Cow::Borrowed(assets::DM_SANS),
                Cow::Borrowed(assets::LILEX_REGULAR),
                Cow::Borrowed(assets::LILEX_BOLD),
                Cow::Borrowed(assets::LILEX_ITALIC),
                Cow::Borrowed(assets::LILEX_BOLD_ITALIC),
            ];
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let fonts: Vec<Cow<'static, [u8]>> = vec![Cow::Borrowed(assets::DM_SANS)];
            cx.text_system()
                .add_fonts(fonts)
                .expect("failed to register gallery fonts");

            theme::init(cx);
            let flags = Options::parse();
            if flags.dark {
                theme::change_mode(ThemeMode::Dark, None, cx);
            }

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(
                    size(px(flags.width.unwrap_or(980.)), px(820.)),
                    cx,
                )),
                window_min_size: Some(size(px(760.), px(600.))),
                ..Default::default()
            };
            let window = cx
                .open_window(options, |window, cx| {
                    let gallery = cx.new(|cx| Gallery::new(flags, cx));
                    cx.new(|cx| gpui_base::Root::new(gallery, window, cx))
                })
                .expect("failed to open component gallery window");
            if flags.tab > 0 {
                cx.spawn(async move |cx| {
                    smol::Timer::after(Duration::from_millis(1500)).await;
                    // Through the untyped handle: the typed one would hold the
                    // root view the keystroke dispatches into.
                    _ = cx.update_window(window.into(), |_, window, cx| {
                        for _ in 0..flags.tab {
                            // Root binds Tab only once something inside it has focus.
                            let tab = gpui::Keystroke::parse("tab").unwrap();
                            if !window.dispatch_keystroke(tab, cx) {
                                window.focus_next(cx);
                            }
                        }
                    });
                })
                .detach();
            }
        });
}

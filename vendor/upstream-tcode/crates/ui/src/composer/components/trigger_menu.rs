use super::super::*;

impl Composer {
    /// Whether the trigger menu is shown and owns Enter/arrows. A `/` or `$`
    /// token that matches nothing is ordinary text (a path, a shell variable,
    /// a provider command Tcode does not know), so no menu opens for it and
    /// Enter submits as usual; only the asynchronous `@` menu keeps an empty
    /// or loading state.
    pub(in super::super) fn menu_visible(&self, cx: &App) -> bool {
        let Some(trigger) = self.active_trigger.as_ref() else {
            return false;
        };
        if self.menu_dismissed {
            return false;
        }
        trigger.kind == TriggerKind::Path || !self.menu_rows(cx).0.is_empty()
    }

    /// Recompute the active trigger from the input text + cursor, resetting the
    /// highlight (and un-dismissing) when the trigger identity changes, and
    /// lazily loading the workspace listing for `@`-mentions.
    pub(in super::super) fn recompute_trigger(&mut self, cx: &mut Context<Self>) {
        let (text, cursor) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.cursor())
        };
        let trigger = detect_composer_trigger(&text, cursor);
        let key = trigger
            .as_ref()
            .map(|t| format!("{:?}\u{1}{}", t.kind, t.query));
        if key != self.menu_last_key {
            self.menu_highlight = 0;
            self.menu_dismissed = false;
            self.menu_last_key = key;
        }
        if matches!(trigger.as_ref().map(|t| t.kind), Some(TriggerKind::Path)) {
            self.ensure_workspace(cx);
        }
        self.active_trigger = trigger;
    }

    /// Load the workspace file/folder listing for the active session cwd in the
    /// background (gitignore-respected), the first time a mention menu opens.
    pub(in super::super) fn ensure_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.workspace_store.read(cx).composer_state().active_cwd else {
            return;
        };
        if self.workspace_loading || self.workspace.as_ref().is_some_and(|(c, _)| *c == cwd) {
            return;
        }
        self.workspace_loading = true;
        let store = self.workspace_store.clone();
        let walked = store.update(cx, |store, cx| store.list_active_workspace(cx));
        cx.spawn(async move |this, cx| {
            let walked = walked.await;
            let _ = this.update(cx, |this, cx| {
                this.workspace = Some((cwd, walked));
                this.workspace_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Build the rows for the currently active trigger menu, plus its empty-state
    /// copy and whether it is still loading.
    pub(in super::super) fn menu_rows(&self, cx: &App) -> (Vec<MenuRow>, String, bool) {
        let Some(trigger) = self.active_trigger.as_ref() else {
            return (Vec::new(), String::new(), false);
        };
        match trigger.kind {
            TriggerKind::Path => {
                let entries = self
                    .workspace
                    .as_ref()
                    .map(|(_, e)| e.as_slice())
                    .unwrap_or(&[]);
                let threads = self.thread_rows(&trigger.query, cx);
                let file_group = (!threads.is_empty()).then_some("composer.group_files");
                let files = filter_entries(entries, &trigger.query, FILE_MENU_ROW_CAP)
                    .into_iter()
                    .map(|e| MenuRow {
                        primary: e.basename.clone(),
                        secondary: e.parent.clone(),
                        icon: if e.is_dir {
                            MenuIcon::Folder
                        } else {
                            MenuIcon::File
                        },
                        accept: MenuAccept::InsertPath(e.rel_path.clone()),
                        group: file_group,
                    });
                let rows = threads.into_iter().chain(files).collect();
                let loading = self.workspace_loading && self.workspace.is_none();
                (rows, crate::tr!("composer.no_files").into_owned(), loading)
            }
            TriggerKind::Skill => {
                // Provider-native skills (Claude `skills` / Codex `skills/list`),
                // fuzzily filtered by the `$` query with no item cap.
                let commands = self
                    .workspace_store
                    .read(cx)
                    .composer_state()
                    .provider_commands;
                let rows =
                    filter_provider_commands(&commands, ProviderCommandKind::Skill, &trigger.query)
                        .into_iter()
                        .map(|c| MenuRow {
                            primary: format!("${}", c.name),
                            secondary: c
                                .description
                                .clone()
                                .unwrap_or_else(|| crate::tr!("composer.run_skill").into_owned()),
                            icon: MenuIcon::Skill,
                            accept: MenuAccept::InsertSkill(c.name.clone()),
                            group: Some("composer.group_skills"),
                        })
                        .collect();
                (rows, String::new(), false)
            }
            TriggerKind::SlashCommand | TriggerKind::SlashModel => {
                let composer = self.workspace_store.read(cx).composer_state();
                let compact_offered = compact_offered(&composer);
                let goal_offered = goal_offered(&composer).is_some();
                let mut builtins: Vec<(&str, Option<&str>, &str, MenuAccept)> = vec![
                    (
                        "model",
                        None,
                        "composer.cmd_model_desc",
                        MenuAccept::OpenModelPicker,
                    ),
                    (
                        "orchestrate",
                        Some("composer.cmd_orchestrate_label"),
                        "composer.cmd_orchestrate_desc",
                        MenuAccept::InsertOrchestrate,
                    ),
                    (
                        "later",
                        None,
                        "composer.cmd_later_desc",
                        MenuAccept::InsertLater,
                    ),
                ];
                if compact_offered {
                    builtins.push((
                        "compact",
                        None,
                        "composer.cmd_compact_desc",
                        MenuAccept::InsertCommand("compact".into()),
                    ));
                }
                if goal_offered {
                    builtins.push((
                        "goal",
                        None,
                        "composer.cmd_goal_desc",
                        MenuAccept::InsertGoal,
                    ));
                }
                let mut rows: Vec<MenuRow> = builtins
                    .into_iter()
                    .filter(|(name, _, _, _)| fuzzy_score(&trigger.query, name).is_some())
                    .map(|(name, label, desc, accept)| MenuRow {
                        primary: format!(
                            "/{}",
                            label
                                .map(|key| crate::tr!(key).into_owned())
                                .unwrap_or_else(|| name.to_string())
                        ),
                        secondary: crate::tr!(desc).into_owned(),
                        icon: MenuIcon::Command,
                        accept,
                        group: Some("composer.group_builtin"),
                    })
                    .collect();
                // Provider-native slash commands (Claude `slash_commands`, which
                // include its skills), shown after the built-in group, fuzzily
                // filtered without truncation. The provider's own `compact` is
                // what the built-in row runs, so it is not listed twice; nor is
                // its own `goal`.
                let commands = composer.provider_commands;
                rows.extend(
                    filter_provider_commands(
                        &commands,
                        ProviderCommandKind::Command,
                        &trigger.query,
                    )
                    .into_iter()
                    .filter(|c| !(compact_offered && c.name == "compact"))
                    .filter(|c| !(goal_offered && c.name == "goal"))
                    .map(|c| MenuRow {
                        primary: format!("/{}", c.name),
                        secondary: c.description.clone().unwrap_or_else(|| {
                            crate::tr!("composer.run_provider_command").into_owned()
                        }),
                        icon: MenuIcon::Command,
                        accept: MenuAccept::InsertCommand(c.name.clone()),
                        group: Some("composer.group_provider"),
                    }),
                );
                (rows, String::new(), false)
            }
        }
    }

    /// Sidebar threads whose title matches an `@` query, for the "Threads"
    /// group: offered once the query names something, to threads whose
    /// provider takes `tcode_control`.
    fn thread_rows(&self, query: &str, cx: &App) -> Vec<MenuRow> {
        let store = self.workspace_store.read(cx);
        let Some(status) = store.session_status().filter(|status| !status.draft) else {
            return Vec::new();
        };
        let (Some(control), Some(session_id)) = (&status.control, store.active_session_id()) else {
            return Vec::new();
        };
        if query.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<_> = store
            .flat_sessions()
            .into_iter()
            .filter(|meta| meta.id != session_id)
            .filter(|meta| {
                !control
                    .references
                    .iter()
                    .any(|reference| reference.thread_id == meta.id)
            })
            .filter_map(|meta| Some((fuzzy_score(query, &meta.title)?, meta)))
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored
            .into_iter()
            .take(THREAD_MENU_ROW_CAP)
            .map(|(_, meta)| MenuRow {
                secondary: meta
                    .project_id
                    .as_deref()
                    .and_then(|id| store.project(id))
                    .map(|project| project.name.clone())
                    .unwrap_or_else(|| crate::tr!("control.reference.secondary").into_owned()),
                primary: meta.title,
                icon: MenuIcon::Thread,
                accept: MenuAccept::ReferenceThread(meta.id),
                group: Some("control.reference.group"),
            })
            .collect()
    }

    /// Replace the active trigger's text range in the input with `replacement`.
    pub(in super::super) fn replace_trigger(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(trigger) = self.active_trigger.clone() else {
            return;
        };
        let replacement = replacement.to_string();
        self.input.update(cx, |state, cx| {
            state.set_selected_range(trigger.range.clone(), cx);
            state.replace(replacement.clone(), window, cx);
        });
    }

    /// Accept the trigger-menu row at `index`.
    pub(in super::super) fn accept_menu(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (rows, _, _) = self.menu_rows(cx);
        let Some(row) = rows.get(index).cloned() else {
            return;
        };
        match &row.accept {
            MenuAccept::InsertPath(path) => {
                let link = format!("{} ", serialize_composer_file_link(path));
                self.replace_trigger(&link, window, cx);
            }
            MenuAccept::ReferenceThread(id) => {
                self.replace_trigger("", window, cx);
                self.reference_thread(id.clone(), window, cx);
            }
            MenuAccept::InsertSkill(name) => self.replace_trigger(&format!("${name} "), window, cx),
            MenuAccept::InsertCommand(name) => {
                self.replace_trigger(&format!("/{name} "), window, cx)
            }
            MenuAccept::InsertOrchestrate => self.replace_trigger("/orchestrate ", window, cx),
            MenuAccept::InsertLater => self.replace_trigger("/later ", window, cx),
            MenuAccept::InsertGoal => self.replace_trigger("/goal ", window, cx),
            MenuAccept::OpenModelPicker => {
                self.replace_trigger("", window, cx);
                self.model_picker_token = self.model_picker_token.wrapping_add(1);
            }
        }
        self.active_trigger = None;
        self.menu_dismissed = true;
        cx.notify();
    }

    /// The floating `@`/`/`/`$` menu, rendered in-flow just above the composer
    /// card. `None` when no trigger is active (or it was dismissed).
    pub(in super::super) fn render_trigger_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.menu_visible(cx) {
            return None;
        }
        let (rows, empty_text, loading) = self.menu_rows(cx);
        let highlight = self.menu_highlight.min(rows.len().saturating_sub(1));

        let compact = self.compact;
        let list = if rows.is_empty() {
            crate::material::empty_row(
                if loading {
                    crate::tr!("composer.searching").into_owned()
                } else {
                    empty_text
                },
                cx,
            )
            .into_any_element()
        } else {
            // Group headers are not selectable: each opens the row that starts
            // its group, so keyboard selection indexes `rows` directly.
            let mut last_group: Option<&'static str> = None;
            let headers: Vec<Option<&'static str>> = rows
                .iter()
                .map(|row| {
                    let group = row.group.filter(|group| last_group != Some(*group));
                    if group.is_some() {
                        last_group = group;
                    }
                    group
                })
                .collect();
            let count = rows.len();
            let rows: Rc<[MenuRow]> = rows.into();
            crate::scroll::VirtualList::measured(
                "composer-trigger-rows",
                count,
                cx.processor(move |_, index: usize, _, cx| {
                    div().when(index + 1 < count, |row| row.pb_0p5()).child(
                        v_flex()
                            .gap_0p5()
                            .when_some(headers[index], |col, group| {
                                col.child(
                                    div()
                                        .px_2()
                                        .pt_1p5()
                                        .pb_0p5()
                                        .text_caption(cx)
                                        .child(crate::tr!(group).into_owned()),
                                )
                            })
                            .child(render_menu_row(
                                &rows[index],
                                index,
                                index == highlight,
                                compact,
                                cx,
                            )),
                    )
                }),
            )
            .reveal(Some(highlight))
            .w_full()
            // About ten rows before the list scrolls, so the menu never
            // covers the conversation; no owner sizes an in-flow menu. The
            // menu's border sits outside this cap.
            .max_h(px(286.))
            .p_1()
            .into_any_element()
        };

        Some(
            crate::material::overlay_contour(div(), cx)
                .rounded(crate::material::radius_overlay(cx))
                .id("composer-trigger-menu")
                .role(Role::ListBox)
                .aria_label(crate::tr!("composer.trigger_results"))
                .w_full()
                .child(list)
                .with_animation(
                    "composer-trigger-menu-pop-in",
                    Animation::new(Duration::from_millis(150)),
                    |element, delta| element.opacity(delta),
                )
                .into_any_element(),
        )
    }
}

fn render_menu_row(
    row: &MenuRow,
    index: usize,
    is_active: bool,
    compact: bool,
    cx: &mut Context<Composer>,
) -> impl IntoElement + use<> {
    let muted = cx.theme().muted_foreground;
    let icon = match row.icon {
        MenuIcon::File => Icon::empty().path("icons/file.svg"),
        MenuIcon::Folder => Icon::empty().path("icons/folder-closed.svg"),
        MenuIcon::Command => Icon::empty().path("icons/box.svg"),
        MenuIcon::Skill => Icon::empty().path("icons/ruler.svg"),
        MenuIcon::Thread => Icon::new(IconName::MessageSquare),
    };
    let accessible_label = crate::tr!(
        "composer.trigger_option",
        primary = row.primary.clone(),
        secondary = row.secondary.clone()
    )
    .into_owned();
    crate::material::menu_item_frame(compact, cx)
        .id(("menu-row", index))
        .debug_selector(move || format!("menu-row-{index}"))
        .role(Role::ListBoxOption)
        .aria_label(accessible_label)
        .aria_selected(is_active)
        .when(is_active, |row| row.aria_active_descendant())
        .flex_none()
        .w_full()
        .items_center()
        .text_body(compact, cx)
        .cursor_pointer()
        .when(is_active, |s| s.bg(cx.theme().list_active))
        .hover(|s| s.bg(cx.theme().muted))
        .child(icon.text_color(muted))
        .child(div().flex_none().child(row.primary.clone()))
        .when(!row.secondary.is_empty(), |this| {
            this.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(muted)
                    .child(crate::widgets::fade_text(row.secondary.clone())),
            )
        })
        .on_mouse_move(cx.listener(move |this, _, _, cx| {
            if this.menu_highlight != index {
                this.menu_highlight = index;
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.accept_menu(index, window, cx);
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};

    fn composer_window(
        compact: bool,
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorkspaceStore>,
        Entity<Composer>,
        &mut VisualTestContext,
    ) {
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(crate::store::tests::scratch_root(
                "trigger-menu-test",
            ))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("trigger-menu".into(), std::env::temp_dir(), cx);
            let timeline = state.residents.live.get(&id).unwrap().timeline.clone();
            (id, timeline)
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &store, "selected conversation", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new_with_layout(store.clone(), compact, window, cx)
        });
        (store, composer, cx)
    }

    #[gpui::test]
    fn the_phone_composer_opens_the_same_trigger_menus(cx: &mut TestAppContext) {
        let (store, composer, cx) = composer_window(true, cx);
        cx.simulate_resize(size(px(393.), px(852.)));
        composer.update_in(cx, |composer, window, cx| {
            let cwd = store.read(cx).composer_state().active_cwd.unwrap();
            composer.workspace = Some((cwd, vec![PathEntry::from_rel("file.rs".into(), false)]));
            window.focus(&composer.input.read(cx).focus_handle(cx), cx);
        });
        for (trigger, first_row) in [("/", "/model"), ("@", "file.rs")] {
            composer.update_in(cx, |composer, window, cx| {
                composer.set_draft("", window, cx)
            });
            cx.simulate_input(trigger);
            cx.update(|window, cx| _ = window.draw(cx));
            assert!(cx.debug_bounds("menu-row-0").is_some(), "{trigger}");
            composer.read_with(cx, |composer, cx| {
                assert_eq!(composer.menu_rows(cx).0[0].primary, first_row, "{trigger}");
            });
        }
    }

    /// A `/` or `$` token that matches nothing is ordinary text: no menu opens,
    /// so Enter submits it instead of accepting a row that does not exist.
    #[gpui::test]
    fn unmatched_command_and_skill_tokens_open_no_menu(cx: &mut TestAppContext) {
        let (_store, composer, cx) = composer_window(false, cx);
        cx.simulate_resize(size(px(800.), px(600.)));
        composer.update_in(cx, |composer, window, cx| {
            window.focus(&composer.input.read(cx).focus_handle(cx), cx);
        });
        for (text, visible) in [
            ("/mod", true),
            ("/nosuchcommand", false),
            ("$HOME", false),
            ("/Users/me/notes.md", false),
        ] {
            composer.update_in(cx, |composer, window, cx| {
                composer.set_draft("", window, cx)
            });
            cx.simulate_input(text);
            cx.update(|window, cx| _ = window.draw(cx));
            composer.read_with(cx, |composer, cx| {
                assert_eq!(composer.menu_visible(cx), visible, "{text:?}");
            });
            assert_eq!(cx.debug_bounds("menu-row-0").is_some(), visible, "{text:?}");
        }
    }

    /// Picking `/compact` from the menu writes the command into the input like
    /// any other; only sending it compacts.
    #[gpui::test]
    fn picking_compact_inserts_the_command_without_running_it(cx: &mut TestAppContext) {
        let (store, composer, cx) = composer_window(false, cx);
        cx.simulate_resize(size(px(800.), px(600.)));
        composer.update_in(cx, |composer, window, cx| {
            window.focus(&composer.input.read(cx).focus_handle(cx), cx);
        });
        assert!(store.read_with(cx, |store, _| compact_offered(&store.composer_state())));
        cx.simulate_input("/comp");
        cx.update(|window, cx| _ = window.draw(cx));
        composer.read_with(cx, |composer, cx| {
            assert_eq!(composer.menu_rows(cx).0[0].primary, "/compact");
        });
        // The action, not a simulated keystroke: the test driver types an
        // unhandled Enter's `\n` into the input, which AppKit does not.
        cx.update(|window, cx| {
            window.dispatch_action(
                Box::new(gpui_base::input::Enter {
                    secondary: false,
                    shift: false,
                }),
                cx,
            )
        });
        composer.read_with(cx, |composer, cx| {
            assert_eq!(composer.input.read(cx).value(), "/compact ");
        });
    }

    #[gpui::test]
    fn arrow_keys_keep_the_highlighted_mention_laid_out(cx: &mut TestAppContext) {
        let (store, composer, cx) = composer_window(false, cx);
        cx.simulate_resize(size(px(800.), px(600.)));
        composer.update_in(cx, |composer, window, cx| {
            let cwd = store.read(cx).composer_state().active_cwd.unwrap();
            let entries = (0..FILE_MENU_ROW_CAP)
                .map(|index| PathEntry::from_rel(format!("file-{index:02}.rs"), false))
                .collect();
            composer.workspace = Some((cwd, entries));
            window.focus(&composer.input.read(cx).focus_handle(cx), cx);
        });
        cx.simulate_input("@");
        cx.update(|window, cx| _ = window.draw(cx));
        let last = FILE_MENU_ROW_CAP - 1;
        let last_row: &'static str = format!("menu-row-{last}").leak();
        assert!(cx.debug_bounds("menu-row-0").is_some());
        assert!(cx.debug_bounds(last_row).is_none());

        for _ in 0..last {
            cx.simulate_keystrokes("down");
            cx.update(|window, cx| _ = window.draw(cx));
        }
        assert!(cx.debug_bounds(last_row).is_some());
        for _ in 0..last {
            cx.simulate_keystrokes("up");
            cx.update(|window, cx| _ = window.draw(cx));
        }
        assert!(cx.debug_bounds("menu-row-0").is_some());
    }
}

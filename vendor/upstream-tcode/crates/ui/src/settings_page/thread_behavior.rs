use super::*;
use tcode_core::{
    project::Project,
    settings::{ProjectSettlementSettings, Settings},
};

pub(crate) fn open_project_rules(
    store: Entity<WorkspaceStore>,
    project: Project,
    window: &mut Window,
    cx: &mut App,
) {
    let editor = cx.new(|cx| ProjectRulesEditor::new(store, project, window, cx));
    let title = editor.read(cx).title();
    window.open_dialog(cx, move |dialog, _, _| {
        let content = editor.clone();
        let save = editor.clone();
        let button_save = editor.clone();
        dialog
            .title(title.clone())
            .content(move |body, _, _| body.child(content.clone()))
            .on_ok(move |_, window, cx| save.update(cx, |editor, cx| editor.save(window, cx)))
            .footer(
                crate::overlay::DialogActions::new()
                    .child(
                        Button::new("project-rules-cancel")
                            .label(crate::tr!("settings.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("project-rules-save")
                            .primary()
                            .label(crate::tr!("settings.auto_settle.save"))
                            .on_click(move |_, window, cx| {
                                if button_save.update(cx, |editor, cx| editor.save(window, cx)) {
                                    window.close_dialog(cx);
                                }
                            }),
                    ),
            )
    });
}

impl SettingsPage {
    pub(super) fn commit_auto_settle_days(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stored = self
            .store
            .read(cx)
            .settings()
            .auto_settle_after_days
            .unwrap_or(3.);
        let parsed = self
            .auto_settle_input
            .state
            .read(cx)
            .value()
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(1., 90.));
        let value = parsed.unwrap_or(stored);
        self.auto_settle_input.dirty = false;
        self.auto_settle_input.push(value.to_string(), window, cx);
        if parsed.is_some() {
            self.dispatch_settings(
                move |store| store.set_auto_settle_after_days(Some(value)),
                cx,
            );
        }
    }

    pub(super) fn render_threads(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let delete_confirm_reset = crate::settings_rows::reset_action(
            "reset-delete-confirm",
            settings.skip_delete_confirmation,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_skip_delete_confirmation(false), cx)
            }),
        );
        let threads = vec![
            self.title_generation_row(cx),
            self.toggle_row(
                "delete-confirm",
                crate::tr!("settings.delete_confirmation.title"),
                crate::tr!("settings.delete_confirmation.description"),
                !settings.skip_delete_confirmation,
                delete_confirm_reset,
                cx,
                |store, checked| store.set_skip_delete_confirmation(!checked),
            ),
        ];
        let settling = self.settling_rows(&settings, cx);
        let panels = self.panel_rows(&settings, cx);
        let model_limits = self.model_limit_rows(&settings, cx);
        let compact = self.window_state.read(cx).compact;
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.threads_section"),
                threads,
                cx,
            ))
            .child(
                crate::settings_rows::settings_group(
                    crate::tr!("settings.settling_section"),
                    settling,
                    cx,
                )
                .child(crate::settings_rows::footnote(
                    crate::tr!("settings.auto_settle.footnote"),
                    compact,
                    cx,
                )),
            )
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.panels_section"),
                panels,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.model_limits_section"),
                model_limits,
                cx,
            ))
    }

    fn panel_rows(&mut self, settings: &Settings, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let auto_open_task_panel_reset = crate::settings_rows::reset_action(
            "reset-auto-open-task-panel",
            settings.auto_open_task_panel,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_auto_open_task_panel(false), cx)
            }),
        );
        let live_command_panel_reset = crate::settings_rows::reset_action(
            "reset-live-command-panel",
            settings.live_command_panel_disabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_live_command_panel_disabled(false), cx)
            }),
        );
        let word_wrap_reset = crate::settings_rows::reset_action(
            "reset-word-wrap",
            settings.word_wrap_diffs,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_word_wrap_diffs(false), cx)
            }),
        );
        vec![
            self.toggle_row(
                "auto-open-task-panel",
                crate::tr!("settings.auto_open_task_panel.title"),
                crate::tr!("settings.auto_open_task_panel.description"),
                settings.auto_open_task_panel,
                auto_open_task_panel_reset,
                cx,
                WorkspaceStore::set_auto_open_task_panel,
            ),
            self.toggle_row(
                "live-command-panel",
                crate::tr!("settings.live_command_panel.title"),
                crate::tr!("settings.live_command_panel.description"),
                !settings.live_command_panel_disabled,
                live_command_panel_reset,
                cx,
                |store, checked| store.set_live_command_panel_disabled(!checked),
            ),
            self.toggle_row(
                "word-wrap",
                crate::tr!("settings.word_wrap.title"),
                crate::tr!("settings.word_wrap.description"),
                settings.word_wrap_diffs,
                word_wrap_reset,
                cx,
                WorkspaceStore::set_word_wrap_diffs,
            ),
        ]
    }

    fn model_limit_rows(&mut self, settings: &Settings, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let abort_on_fallback_reset = crate::settings_rows::reset_action(
            "reset-abort-on-model-fallback",
            !settings.abort_on_model_fallback,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_abort_on_model_fallback(true), cx)
            }),
        );
        let mut rows = vec![self.toggle_row(
            "abort-on-model-fallback",
            crate::tr!("settings.abort_on_model_fallback.title"),
            crate::tr!("settings.abort_on_model_fallback.description"),
            settings.abort_on_model_fallback,
            abort_on_fallback_reset,
            cx,
            WorkspaceStore::set_abort_on_model_fallback,
        )];
        if settings.abort_on_model_fallback {
            let advisor_reset = crate::settings_rows::reset_action(
                "reset-fallback-review-advisor",
                settings.fallback_review_advisor,
                cx.listener(|this, _, _, cx| {
                    this.dispatch_settings(|store| store.set_fallback_review_advisor(false), cx)
                }),
            );
            rows.push(self.toggle_row(
                "fallback-review-advisor",
                crate::tr!("settings.fallback_review_advisor.title"),
                crate::tr!("settings.fallback_review_advisor.description"),
                settings.fallback_review_advisor,
                advisor_reset,
                cx,
                WorkspaceStore::set_fallback_review_advisor,
            ));
            rows.push(self.fallback_review_model_row(cx));
        }
        let resume_on_limit_reset_reset = crate::settings_rows::reset_action(
            "reset-resume-on-limit-reset",
            !settings.resume_on_limit_reset,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_resume_on_limit_reset(true), cx)
            }),
        );
        rows.push(self.toggle_row(
            "resume-on-limit-reset",
            crate::tr!("settings.resume_on_limit_reset.title"),
            crate::tr!("settings.resume_on_limit_reset.description"),
            settings.resume_on_limit_reset,
            resume_on_limit_reset_reset,
            cx,
            WorkspaceStore::set_resume_on_limit_reset,
        ));
        rows
    }

    /// Auto-settle, its days and merge rules, and the per-project overrides:
    /// a phone lists every project as a link to its rules page; a wide window
    /// adds overrides from a menu and edits them in a dialog.
    fn settling_rows(&mut self, settings: &Settings, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let reset = crate::settings_rows::reset_action(
            "reset-auto-settle",
            settings.auto_settle_after_days != Some(3.),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_auto_settle_after_days(Some(3.)), cx)
            }),
        );
        let mut rows = vec![self.toggle_row(
            "auto-settle-inactive",
            crate::tr!("settings.auto_settle.title"),
            crate::tr!("settings.auto_settle.description"),
            settings.auto_settle_after_days.is_some(),
            reset,
            cx,
            |store, checked| store.set_auto_settle_after_days(checked.then_some(3.)),
        )];
        if settings.auto_settle_after_days.is_some() {
            rows.push(
                crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
                    .child(crate::settings_rows::row_labels(
                        crate::tr!("settings.auto_settle.days"),
                        Some(crate::tr!("settings.auto_settle.days_description").into()),
                        None,
                        self.window_state.read(cx).compact,
                        cx,
                    ))
                    .child(
                        crate::widgets::input::NumberInput::new(&self.auto_settle_input.state)
                            .accessibility_label(crate::tr!("settings.auto_settle.days")),
                    )
                    .into_any_element(),
            );
        }
        let merge_reset = crate::settings_rows::reset_action(
            "reset-auto-settle-on-merge",
            settings.auto_settle_on_merge,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_auto_settle_on_merge(false), cx)
            }),
        );
        rows.push(self.toggle_row(
            "auto-settle-on-merge",
            crate::tr!("settings.auto_settle.on_merge"),
            crate::tr!("settings.auto_settle.on_merge_description"),
            settings.auto_settle_on_merge,
            merge_reset,
            cx,
            |store, checked| store.set_auto_settle_on_merge(checked),
        ));
        let projects = self.store.read(cx).projects();
        let summary = |project: &Project| -> Option<SharedString> {
            let rules = settings.project_settlement_overrides.get(&project.id)?;
            let parts: Vec<SharedString> = rules
                .auto_settle_after_days
                .map(days_label)
                .into_iter()
                .chain(rules.auto_settle_on_merge.map(|on| {
                    crate::tr!(
                        "settings.auto_settle.summary_merge",
                        value = on_off_label(on).to_string()
                    )
                    .into_owned()
                    .into()
                }))
                .collect();
            (!parts.is_empty()).then(|| parts.join(" · ").into())
        };
        if self.window_state.read(cx).compact {
            // Phone: every project is a row that pushes its rules page; there
            // is no dropdown or dialog to pick one from.
            rows.push(
                crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
                    .child(crate::settings_rows::row_labels(
                        crate::tr!("settings.auto_settle.overrides"),
                        Some(crate::tr!("settings.auto_settle.overrides_description").into()),
                        None,
                        self.window_state.read(cx).compact,
                        cx,
                    ))
                    .into_any_element(),
            );
            for project in projects {
                let summary = summary(&project);
                let edit = project.clone();
                rows.push(
                    crate::material::accessible_clickable(
                        crate::material::group_row(cx),
                        SharedString::from(format!("project-rules-{}", project.id)),
                        Role::Button,
                        SharedString::from(project.name.clone()),
                        cx,
                    )
                    .cursor_pointer()
                    .hover(|row| row.bg(cx.theme().list_hover))
                    .text_body(true, cx)
                    .child(crate::project_icon::artwork(&project, 16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(project.name.clone())),
                    )
                    .children(summary.map(|summary| div().text_secondary(true, cx).child(summary)))
                    .child(
                        Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit_project_rules(edit.clone(), window, cx)
                    }))
                    .into_any_element(),
                );
            }
        } else {
            let options = projects
                .iter()
                .filter(|project| {
                    !settings
                        .project_settlement_overrides
                        .contains_key(&project.id)
                })
                .map(|project| SelectRowOption {
                    id: project.id.clone().into(),
                    label: project.name.clone().into(),
                    value: project.clone(),
                    selected: false,
                    description: None,
                })
                .collect();
            rows.push(crate::settings_rows::select_row(
                "add-project-rules",
                "project-rules-popover",
                "project-rules-menu",
                240.,
                crate::tr!("settings.auto_settle.overrides")
                    .into_owned()
                    .into(),
                crate::tr!("settings.auto_settle.overrides_description")
                    .into_owned()
                    .into(),
                crate::tr!("settings.auto_settle.add_override")
                    .into_owned()
                    .into(),
                options,
                None,
                self.window_state.read(cx).compact,
                {
                    let page = cx.entity();
                    move |project, window, cx| {
                        page.update(cx, |page, cx| page.edit_project_rules(project, window, cx))
                    }
                },
                cx,
            ));
            for project in projects.into_iter().filter(|project| {
                settings
                    .project_settlement_overrides
                    .contains_key(&project.id)
            }) {
                let summary = summary(&project).unwrap_or_default();
                let edit = project.clone();
                let id = project.id.clone();
                rows.push(
                    crate::material::group_row(cx)
                        .text_body(false, cx)
                        .child(crate::project_icon::artwork(&project, 16.))
                        .child(div().flex_1().child(project.name))
                        .child(div().text_secondary(false, cx).child(summary))
                        .child(
                            Button::new(SharedString::from(format!("edit-rules-{id}")))
                                .ghost()
                                .xsmall()
                                .label(crate::tr!("settings.auto_settle.edit"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_project_rules(edit.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("remove-rules-{id}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip(crate::tr!("settings.auto_settle.remove_override"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.dispatch_settings(
                                        |store| store.set_project_settlement(id.clone(), None),
                                        cx,
                                    )
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        rows
    }

    fn edit_project_rules(
        &mut self,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.read(cx).compact {
            self.project_rules_editor =
                Some(cx.new(|cx| ProjectRulesEditor::new(self.store.clone(), project, window, cx)));
            self.project_rules_subscription = self.project_rules_editor.as_ref().map(|editor| {
                cx.subscribe(editor, |this, _, _: &gpui::DismissEvent, cx| {
                    this.window_state.update(cx, |state, cx| state.back(cx));
                    this.section = Section::Threads;
                    this.project_rules_editor = None;
                    cx.notify();
                })
            });
            self.section = Section::ProjectThreadRules;
            self.window_state.update(cx, |state, cx| {
                state.go(crate::window_state::Destination::SettingsThreadRules, cx)
            });
            cx.notify();
        } else {
            open_project_rules(self.store.clone(), project, window, cx);
        }
    }
}

fn days_label(value: Option<f64>) -> SharedString {
    value.map_or_else(
        || crate::tr!("settings.auto_settle.never").into_owned().into(),
        |days| {
            if days == 1. {
                crate::tr!("settings.auto_settle.days_value_one")
            } else {
                crate::tr!("settings.auto_settle.days_value", count = days)
            }
            .into_owned()
            .into()
        },
    )
}

pub(super) struct ProjectRulesEditor {
    store: Entity<WorkspaceStore>,
    pub(super) project: Project,
    draft: ProjectSettlementSettings,
    days: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectRulesEditor {
    fn new(
        store: Entity<WorkspaceStore>,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = store.read(cx).settings();
        let draft = settings
            .project_settlement_overrides
            .get(&project.id)
            .cloned()
            .unwrap_or_default();
        let value = draft
            .auto_settle_after_days
            .flatten()
            .or(settings.auto_settle_after_days)
            .unwrap_or(3.);
        let days = cx.new(|cx| {
            InputState::new(window, cx)
                .step(1.)
                .min(1.)
                .max(90.)
                .default_value(value.to_string())
        });
        let subscriptions = vec![
            cx.subscribe_in(&days, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    this.commit_days(window, cx);
                }
            }),
        ];
        Self {
            store,
            project,
            draft,
            days,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn title(&self) -> SharedString {
        crate::tr!(
            "settings.auto_settle.dialog_title",
            project = self.project.name.clone()
        )
        .into_owned()
        .into()
    }

    fn commit_days(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let stored = self.draft.auto_settle_after_days.flatten().unwrap_or(3.);
        let parsed = self
            .days
            .read(cx)
            .value()
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(1., 90.));
        let value = parsed.unwrap_or(stored);
        self.days.update(cx, |input, cx| {
            input.set_value(value.to_string(), window, cx)
        });
        if matches!(self.draft.auto_settle_after_days, Some(Some(_))) {
            self.draft.auto_settle_after_days = Some(Some(value));
        }
        parsed.is_some()
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.commit_days(window, cx);
        let value =
            (self.draft != ProjectSettlementSettings::default()).then(|| self.draft.clone());
        self.store.update(cx, |store, _| {
            store.set_project_settlement(self.project.id.clone(), value)
        });
        true
    }
}

impl Render for ProjectRulesEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.store.read(cx).settings();
        let default = days_label(settings.auto_settle_after_days);
        let days_choices = vec![
            (
                None,
                crate::tr!(
                    "settings.auto_settle.use_default",
                    value = default.to_string()
                )
                .into_owned(),
            ),
            (
                Some(None),
                crate::tr!("settings.auto_settle.never").into_owned(),
            ),
            (
                Some(Some(
                    self.draft.auto_settle_after_days.flatten().unwrap_or(3.),
                )),
                crate::tr!("settings.auto_settle.after_days").into_owned(),
            ),
        ];
        let selected = self.draft.auto_settle_after_days;
        let days_index = match selected {
            None => 0,
            Some(None) => 1,
            Some(Some(_)) => 2,
        };
        let merge_choices = vec![
            (
                None,
                crate::tr!(
                    "settings.auto_settle.use_default",
                    value = on_off_label(settings.auto_settle_on_merge).to_string()
                )
                .into_owned(),
            ),
            (Some(true), on_off_label(true).into()),
            (Some(false), on_off_label(false).into()),
        ];
        let merge_index = match self.draft.auto_settle_on_merge {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        let compact = crate::window_seam::window_is_compact(window, cx);
        let mut content = v_flex()
            .gap_3()
            .child(
                div()
                    .text_body(compact, cx)
                    .child(crate::tr!("settings.auto_settle.title")),
            )
            .child(rule_selector(
                "project-auto-settle",
                days_choices,
                days_index,
                |editor, value| editor.draft.auto_settle_after_days = value,
                compact,
                cx,
            ));
        if matches!(self.draft.auto_settle_after_days, Some(Some(_))) {
            content = content
                .child(
                    div()
                        .text_caption(cx)
                        .child(crate::tr!("settings.auto_settle.days")),
                )
                .child(
                    crate::widgets::input::NumberInput::new(&self.days)
                        .accessibility_label(crate::tr!("settings.auto_settle.days")),
                );
        }
        content = content
            .child(
                div()
                    .text_body(compact, cx)
                    .child(crate::tr!("settings.auto_settle.on_merge")),
            )
            .child(rule_selector(
                "project-auto-settle-merge",
                merge_choices,
                merge_index,
                |editor, value| editor.draft.auto_settle_on_merge = value,
                compact,
                cx,
            ));
        if compact {
            content = content.child(
                Button::new("save-project-rules")
                    .label(crate::tr!("settings.auto_settle.save"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.save(window, cx);
                        cx.emit(gpui::DismissEvent);
                    })),
            );
        }
        content
    }
}

fn on_off_label(on: bool) -> SharedString {
    if on {
        crate::tr!("settings.auto_settle.on")
    } else {
        crate::tr!("settings.auto_settle.off")
    }
    .into_owned()
    .into()
}

/// One project rule's choices: a phone page lists them; a dialog opens a dropdown.
fn rule_selector<T: Clone + 'static>(
    id: &'static str,
    choices: Vec<(T, String)>,
    selected: usize,
    pick: fn(&mut ProjectRulesEditor, T),
    compact: bool,
    cx: &mut Context<ProjectRulesEditor>,
) -> AnyElement {
    let editor = cx.entity();
    if compact {
        let rows = choices
            .into_iter()
            .enumerate()
            .map(|(index, (value, label))| {
                let checked = index == selected;
                let editor = editor.clone();
                crate::material::accessible_clickable(
                    crate::material::group_row(cx),
                    (SharedString::from(format!("{id}-choice")), index),
                    Role::Button,
                    SharedString::from(label.clone()),
                    cx,
                )
                .aria_selected(checked)
                .cursor_pointer()
                .hover(|row| row.bg(cx.theme().list_hover))
                .text_body(true, cx)
                .child(div().flex_1().child(label))
                .when(checked, |row| {
                    row.child(Icon::new(IconName::Check).text_color(cx.theme().primary))
                })
                .on_click(move |_, _, cx| {
                    editor.update(cx, |editor, cx| {
                        pick(editor, value.clone());
                        cx.notify();
                    })
                })
                .into_any_element()
            })
            .collect();
        return crate::material::grouped(rows, cx).into_any_element();
    }
    let trigger_label = choices
        .get(selected)
        .map(|(_, label)| label.clone())
        .unwrap_or_default();
    crate::material::overlay_popover(id, cx)
        .trigger(
            Button::new(SharedString::from(format!("{id}-choice")))
                .ghost()
                .outline()
                .compact()
                .child(
                    gpui_base::h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(trigger_label)
                        .child(
                            Icon::new(IconName::ChevronDown)
                                .text_color(cx.theme().muted_foreground),
                        ),
                ),
        )
        .content(move |_, _, cx| {
            let popover = cx.entity();
            // The menu is at least as wide as its trigger; gpui-base's Select is not adopted yet.
            v_flex().p_1().min_w(px(240.)).gap_0p5().children(
                choices
                    .clone()
                    .into_iter()
                    .enumerate()
                    .map(|(index, (value, label))| {
                        let editor = editor.clone();
                        let popover = popover.clone();
                        let checked = index == selected;
                        crate::material::accessible_clickable(
                            crate::material::menu_item_frame(false, cx),
                            (SharedString::from(format!("{id}-option")), index),
                            Role::MenuItem,
                            SharedString::from(label.clone()),
                            cx,
                        )
                        .aria_selected(checked)
                        .w_full()
                        .text_body(false, cx)
                        .cursor_pointer()
                        .hover(|item| item.bg(cx.theme().muted))
                        .child(div().flex_1().child(label))
                        .when(checked, |item| item.child(Icon::new(IconName::Check)))
                        .on_click(move |_, window, cx| {
                            editor.update(cx, |editor, cx| {
                                pick(editor, value.clone());
                                cx.notify();
                            });
                            popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                        })
                    }),
            )
        })
        .into_any_element()
}
impl gpui::EventEmitter<gpui::DismissEvent> for ProjectRulesEditor {}

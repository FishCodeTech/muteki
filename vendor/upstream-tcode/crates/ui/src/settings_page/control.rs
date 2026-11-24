use super::*;
use tcode_core::control::{ControlGrant, SettlementAutomation};
use tcode_core::project::Project;
use tcode_protocol::ControlActivityEntry;

/// How many recent `tcode_control` writes the section lists.
const ACTIVITY_LIMIT: u32 = 50;

#[derive(Default)]
pub(super) struct ControlSection {
    activity: Option<Result<Vec<ControlActivityEntry>, String>>,
    activity_task: Option<gpui::Task<()>>,
    /// Latches the one activity query fired when the section opens.
    activity_requested: bool,
    /// The project whose grant the compact page shows open.
    expanded_project: Option<String>,
}

impl ControlSection {
    pub(super) fn hidden(&mut self) {
        self.activity_requested = false;
    }
}

impl SettingsPage {
    fn refresh_control_activity(&mut self, cx: &mut Context<Self>) {
        self.control.activity_requested = true;
        let query = self
            .store
            .update(cx, |store, cx| store.control_activity(ACTIVITY_LIMIT, cx));
        self.control.activity_task = Some(cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                this.control.activity = Some(result);
                this.control.activity_task = None;
                cx.notify();
            });
        }));
    }

    fn project_grant_change(
        &self,
        project_id: String,
        cx: &Context<Self>,
    ) -> crate::control::GrantChange {
        let page = cx.entity();
        Rc::new(move |grant, _, cx| {
            let project_id = project_id.clone();
            page.update(cx, |page, cx| {
                page.dispatch_settings(
                    move |store| store.set_control_project_grant(project_id, Some(grant)),
                    cx,
                )
            })
        })
    }

    pub(super) fn render_control(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        if !self.control.activity_requested {
            self.refresh_control_activity(cx);
        }
        let settings = self.store.read(cx).settings();
        let control = settings.control.clone();
        let compact = self.window_state.read(cx).compact;
        let projects = self.store.read(cx).projects();

        let enabled_reset = crate::settings_rows::reset_action(
            "reset-control-enabled",
            !control.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_control_enabled(true), cx)
            }),
        );
        let automation = settings.settlement_automation;
        let automation_reset = crate::settings_rows::reset_action(
            "reset-settlement-automation",
            !automation.is_default(),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_settlement_automation(SettlementAutomation::default()),
                    cx,
                )
            }),
        );
        let automation_label = |value: SettlementAutomation| -> SharedString {
            match value {
                SettlementAutomation::Off => crate::tr!("control.settings.automation_off"),
                SettlementAutomation::Suggest => crate::tr!("control.settings.automation_suggest"),
                SettlementAutomation::ExplicitSignals => {
                    crate::tr!("control.settings.automation_explicit")
                }
            }
            .into_owned()
            .into()
        };
        let automation_options = [
            (
                SettlementAutomation::Off,
                "settlement-automation-off",
                "control.settings.automation_off_description",
            ),
            (
                SettlementAutomation::Suggest,
                "settlement-automation-suggest",
                "control.settings.automation_suggest_description",
            ),
            (
                SettlementAutomation::ExplicitSignals,
                "settlement-automation-explicit",
                "control.settings.automation_explicit_description",
            ),
        ]
        .into_iter()
        .map(|(value, id, description)| SelectRowOption {
            value,
            id: id.into(),
            label: automation_label(value),
            description: Some(crate::tr!(description).into_owned().into()),
            selected: value == automation,
        })
        .collect();
        let general = vec![
            self.toggle_row(
                "control-enabled",
                crate::tr!("control.settings.enabled"),
                crate::tr!("control.settings.enabled_description"),
                control.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_control_enabled,
            ),
            crate::settings_rows::select_row(
                "settlement-automation-dropdown",
                "settlement-automation-popover",
                "settlement-automation-menu",
                320.,
                crate::tr!("control.settings.automation")
                    .into_owned()
                    .into(),
                crate::tr!("control.settings.automation_description")
                    .into_owned()
                    .into(),
                automation_label(automation),
                automation_options,
                automation_reset,
                self.window_state.read(cx).compact,
                {
                    let page = cx.entity();
                    move |value, _, cx| {
                        page.update(cx, |page, cx| {
                            page.dispatch_settings(
                                move |store| store.set_settlement_automation(value),
                                cx,
                            )
                        })
                    }
                },
                cx,
            ),
        ];

        let default_reset = crate::settings_rows::reset_action(
            "reset-control-default-grant",
            control.default_grant != ControlGrant::default_grant(),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_control_default_grant(ControlGrant::default_grant()),
                    cx,
                )
            }),
        );
        let page = cx.entity();
        let default_editor = crate::control::grant_editor(
            "control-default-grant",
            &control.default_grant,
            &projects,
            compact,
            Rc::new(move |grant, _, cx| {
                page.update(cx, |page, cx| {
                    page.dispatch_settings(move |store| store.set_control_default_grant(grant), cx)
                })
            }),
            cx,
        );

        let project_rows = self.control_project_rows(&control, projects, compact, cx);
        let thread_rows = self.control_thread_rows(compact, cx);
        let activity_rows = self.control_activity_rows(compact, cx);
        let default_heading = vec![
            crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
                .child(crate::settings_rows::row_labels(
                    crate::control::grant_summary(&control.default_grant),
                    Some(crate::tr!("control.settings.default_grant_description").into()),
                    default_reset,
                    self.window_state.read(cx).compact,
                    cx,
                ))
                .into_any_element(),
        ];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.title"),
                general,
                cx,
            ))
            .child(
                crate::settings_rows::settings_group(
                    crate::tr!("control.settings.default_grant"),
                    default_heading,
                    cx,
                )
                .gap_2()
                .child(default_editor),
            )
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.settings.overrides"),
                project_rows,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.settings.threads"),
                thread_rows,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.settings.activity"),
                activity_rows,
                cx,
            ))
    }

    fn control_project_rows(
        &mut self,
        control: &tcode_core::control::ControlSettings,
        projects: Vec<Project>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        if compact {
            // Phone: every project is a row that opens its grant in place; there
            // is no dropdown or dialog to pick one from.
            rows.push(
                crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
                    .child(crate::settings_rows::row_labels(
                        crate::tr!("control.settings.overrides"),
                        Some(crate::tr!("control.settings.overrides_description").into()),
                        None,
                        self.window_state.read(cx).compact,
                        cx,
                    ))
                    .into_any_element(),
            );
            for project in &projects {
                let own = control.project_overrides.get(&project.id);
                let open = self.control.expanded_project.as_deref() == Some(project.id.as_str());
                let toggle = project.id.clone();
                rows.push(
                    crate::material::accessible_clickable(
                        crate::material::group_row(cx),
                        SharedString::from(format!("control-project-{}", project.id)),
                        Role::Button,
                        SharedString::from(project.name.clone()),
                        cx,
                    )
                    .aria_expanded(open)
                    .cursor_pointer()
                    .hover(|row| row.bg(cx.theme().list_hover))
                    .text_body(true, cx)
                    .child(crate::project_icon::artwork(project, 16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(project.name.clone())),
                    )
                    .children(own.map(|grant| {
                        div()
                            .text_secondary(true, cx)
                            .child(crate::control::grant_summary(grant))
                    }))
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.control.expanded_project =
                            if this.control.expanded_project.as_ref() == Some(&toggle) {
                                None
                            } else {
                                Some(toggle.clone())
                            };
                        cx.notify();
                    }))
                    .into_any_element(),
                );
                if open {
                    let grant = own
                        .cloned()
                        .unwrap_or_else(|| control.default_grant.clone());
                    let id = project.id.clone();
                    rows.push(
                        crate::material::group_row_stacked(cx)
                            .child(crate::control::grant_editor(
                                SharedString::from(format!("control-project-grant-{}", project.id)),
                                &grant,
                                &projects,
                                true,
                                self.project_grant_change(project.id.clone(), cx),
                                cx,
                            ))
                            .when(own.is_some(), |row| {
                                row.child(
                                    Button::new(SharedString::from(format!(
                                        "control-project-default-{}",
                                        project.id
                                    )))
                                    .ghost()
                                    .label(crate::tr!("control.settings.remove_override"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let id = id.clone();
                                        this.dispatch_settings(
                                            move |store| store.set_control_project_grant(id, None),
                                            cx,
                                        )
                                    })),
                                )
                            })
                            .into_any_element(),
                    );
                }
            }
            return rows;
        }
        let options = projects
            .iter()
            .filter(|project| !control.project_overrides.contains_key(&project.id))
            .map(|project| SelectRowOption {
                id: SharedString::from(format!("control-add-{}", project.id)),
                label: project.name.clone().into(),
                value: project.clone(),
                selected: false,
                description: None,
            })
            .collect();
        let default_grant = control.default_grant.clone();
        let store = self.store.clone();
        rows.push(crate::settings_rows::select_row(
            "add-project-grant",
            "project-grant-popover",
            "project-grant-menu",
            240.,
            crate::tr!("control.settings.overrides").into_owned().into(),
            crate::tr!("control.settings.overrides_description")
                .into_owned()
                .into(),
            crate::tr!("control.settings.add_override")
                .into_owned()
                .into(),
            options,
            None,
            self.window_state.read(cx).compact,
            {
                let page = cx.entity();
                move |project, window, cx| {
                    let grant = default_grant.clone();
                    let id = project.id.clone();
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(
                            move |store| store.set_control_project_grant(id, Some(grant)),
                            cx,
                        )
                    });
                    open_project_grant(store.clone(), project, window, cx);
                }
            },
            cx,
        ));
        for project in projects
            .iter()
            .filter(|project| control.project_overrides.contains_key(&project.id))
        {
            let summary = control
                .project_overrides
                .get(&project.id)
                .map(crate::control::grant_summary)
                .unwrap_or_default();
            let edit = project.clone();
            let id = project.id.clone();
            let store = self.store.clone();
            rows.push(
                crate::material::group_row(cx)
                    .text_body(false, cx)
                    .child(crate::project_icon::artwork(project, 16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(project.name.clone())),
                    )
                    .child(div().text_secondary(false, cx).child(summary))
                    .child(
                        Button::new(SharedString::from(format!("edit-grant-{id}")))
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("control.settings.edit"))
                            .on_click(move |_, window, cx| {
                                open_project_grant(store.clone(), edit.clone(), window, cx)
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("remove-grant-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(crate::tr!("control.settings.remove_override"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = id.clone();
                                this.dispatch_settings(
                                    move |store| store.set_control_project_grant(id, None),
                                    cx,
                                )
                            })),
                    )
                    .into_any_element(),
            );
        }
        rows
    }

    fn control_thread_rows(&self, compact: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let threads: Vec<SessionMeta> = self
            .store
            .read(cx)
            .flat_sessions()
            .into_iter()
            .filter(|meta| meta.control_grant.is_some())
            .collect();
        if threads.is_empty() {
            return vec![
                crate::material::empty_row(crate::tr!("control.settings.threads_empty"), cx)
                    .into_any_element(),
            ];
        }
        threads
            .into_iter()
            .map(|meta| {
                let summary = meta
                    .control_grant
                    .as_ref()
                    .map(crate::control::grant_summary)
                    .unwrap_or_default();
                let id = meta.id.clone();
                crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
                    .child(crate::settings_rows::row_labels(
                        meta.title.clone(),
                        Some(summary.into()),
                        None,
                        self.window_state.read(cx).compact,
                        cx,
                    ))
                    .child(
                        Button::new(SharedString::from(format!(
                            "revoke-thread-grant-{}",
                            meta.id
                        )))
                        .ghost()
                        .when(!compact, |button| button.xsmall())
                        .label(crate::tr!("control.settings.revoke"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = id.clone();
                            this.store
                                .update(cx, |store, _| store.clear_control_grant(id));
                        })),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn control_activity_rows(&self, compact: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let refresh = Button::new("control-activity-refresh")
            .ghost()
            .xsmall()
            .icon(IconName::RefreshCw)
            .tooltip(crate::tr!("control.settings.activity_refresh"))
            .aria_label(crate::tr!("control.settings.activity_refresh"))
            .disabled(self.control.activity_task.is_some())
            .on_click(cx.listener(|this, _, _, cx| this.refresh_control_activity(cx)));
        let entries = match &self.control.activity {
            None => {
                return vec![
                    crate::material::empty_row(crate::tr!("control.settings.activity_loading"), cx)
                        .into_any_element(),
                ];
            }
            Some(Err(error)) => {
                return vec![
                    crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
                        .child(
                            div()
                                .flex_1()
                                .text_secondary(compact, cx)
                                .child(error.clone()),
                        )
                        .child(refresh)
                        .into_any_element(),
                ];
            }
            Some(Ok(entries)) => entries,
        };
        if entries.is_empty() {
            return vec![
                crate::material::group_row(cx)
                    .child(
                        div()
                            .flex_1()
                            .text_secondary(compact, cx)
                            .child(crate::tr!("control.settings.activity_empty")),
                    )
                    .child(refresh)
                    .into_any_element(),
            ];
        }
        let now = now_secs();
        let mut rows = vec![
            crate::material::group_row(cx)
                .child(div().flex_1())
                .child(refresh)
                .into_any_element(),
        ];
        rows.extend(entries.iter().map(|entry| {
            let target = if entry.target_title.is_empty() {
                crate::tr!("control.settings.deleted_thread").into_owned()
            } else {
                entry.target_title.clone()
            };
            let title = crate::tr!(
                "control.settings.activity_row",
                actor = entry.action.actor_title.as_str(),
                target = target
            )
            .into_owned();
            let mut action = crate::control::action_label(&entry.action.action);
            if let Some(detail) = entry.action.detail.as_deref().filter(|d| !d.is_empty()) {
                action.push_str(&format!(" ({detail})"));
            }
            crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
                .child(crate::settings_rows::row_labels(
                    title,
                    Some(action.into()),
                    None,
                    self.window_state.read(cx).compact,
                    cx,
                ))
                .child(
                    div()
                        .flex_none()
                        .text_secondary(compact, cx)
                        .child(humanize_ago(now.saturating_sub(entry.at / 1000))),
                )
                .into_any_element()
        }));
        rows
    }
}

/// A project's grant in a dialog; every edit is saved as it is made.
pub(crate) fn open_project_grant(
    store: Entity<WorkspaceStore>,
    project: Project,
    window: &mut Window,
    cx: &mut App,
) {
    let editor = cx.new(|cx| ProjectGrantEditor::new(store, project, cx));
    let title: SharedString = crate::tr!(
        "control.settings.dialog_title",
        project = editor.read(cx).project.name.clone()
    )
    .into_owned()
    .into();
    window.open_dialog(cx, move |dialog, _, _| {
        let content = editor.clone();
        let reset = editor.clone();
        dialog
            .title(title.clone())
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                crate::overlay::DialogActions::new()
                    .child(
                        Button::new("project-grant-default")
                            .label(crate::tr!("control.settings.remove_override"))
                            .on_click(move |_, window, cx| {
                                reset.update(cx, |editor, cx| editor.use_default(cx));
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("project-grant-done")
                            .primary()
                            .label(crate::tr!("control.settings.done"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    });
}

struct ProjectGrantEditor {
    store: Entity<WorkspaceStore>,
    project: Project,
    _subscription: Subscription,
}

impl ProjectGrantEditor {
    fn new(store: Entity<WorkspaceStore>, project: Project, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            project,
            _subscription: subscription,
        }
    }

    fn use_default(&mut self, cx: &mut Context<Self>) {
        let id = self.project.id.clone();
        self.store.update(cx, |store, cx| {
            store.set_control_project_grant(id, None);
            store.local_settings_changed(cx);
        });
    }
}

impl Render for ProjectGrantEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let control = store.settings().control;
        let projects = store.projects();
        let grant = control
            .project_overrides
            .get(&self.project.id)
            .cloned()
            .unwrap_or(control.default_grant);
        let compact = crate::window_seam::window_is_compact(window, cx);
        let store = self.store.clone();
        let project_id = self.project.id.clone();
        crate::control::grant_editor(
            "project-grant-dialog",
            &grant,
            &projects,
            compact,
            Rc::new(move |grant, _, cx| {
                let project_id = project_id.clone();
                store.update(cx, |store, cx| {
                    store.set_control_project_grant(project_id, Some(grant));
                    store.local_settings_changed(cx);
                })
            }),
            cx,
        )
    }
}

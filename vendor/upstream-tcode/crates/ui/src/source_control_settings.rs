//! Settings → Source Control: every host Tcode reads pull requests from, whatever its kind. The
//! host decides which hosts exist, where each credential comes from and what is wrong; this
//! panel shows it and sends the user's choices.

use crate::{
    icon::Icon,
    overlay::{DialogActions, OverlayExt as _},
    sizing::Sizable as _,
    store::{StoreChange, TopicKind, WorkspaceStore},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Tooltip,
        button::{Button, ButtonVariant, ButtonVariants as _},
        choice::{Choice, ChoiceGroup},
        input::{Input, InputEvent, InputState},
        menu::DropdownMenu as _,
        switch::Switch,
    },
};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};
use std::collections::{BTreeMap, BTreeSet};
use tcode_core::{
    pull_request::{HostKind, HostRefusal},
    settings::{CredentialSource, HostProblem, HostSettings, HostStatus, SourceControlSettings},
};

#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = source_control_settings, no_json)]
struct ChooseAccount {
    host: String,
    kind: HostKind,
    account: Option<String>,
}

#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = source_control_settings, no_json)]
struct ChooseKind {
    kind: HostKind,
}

/// A host kind's name in keys: `github`, `forgejo`, `gitea`, `azure_devops`.
fn slug(kind: HostKind) -> String {
    kind.terms().name.to_ascii_lowercase().replace(' ', "_")
}

/// What Add host's input holds for a kind before anything is typed: its public host, or
/// nothing where a host is an organization the user names.
fn prefill(kind: HostKind) -> &'static str {
    if kind.organizations() {
        ""
    } else {
        kind.public_host()
    }
}

fn mark(kind: HostKind) -> Icon {
    Icon::empty().path(kind.terms().mark)
}

/// The hosts to list, each with its kind: configured, reported by the host, and github.com;
/// by kind, then the kind's public host, then by name.
fn listed(settings: &SourceControlSettings) -> Vec<(String, HostKind)> {
    let names: BTreeSet<_> = settings
        .hosts
        .keys()
        .chain(settings.status.keys())
        .cloned()
        .chain(std::iter::once("github.com".to_owned()))
        .collect();
    let mut listed: Vec<_> = names
        .into_iter()
        .map(|host| {
            let kind = settings.kind(&host);
            (host, kind)
        })
        .collect();
    listed.sort_by(|(left, left_kind), (right, right_kind)| {
        (left_kind, left != left_kind.public_host(), left).cmp(&(
            right_kind,
            right != right_kind.public_host(),
            right,
        ))
    });
    listed
}

fn source_slot(source: &CredentialSource) -> String {
    match source {
        CredentialSource::Saved
        | CredentialSource::SavedBasic { .. }
        | CredentialSource::SavedBearer => crate::tr!("source_control.order_saved").into_owned(),
        CredentialSource::Env { name } => name.clone(),
        CredentialSource::Cli { tool } => {
            crate::tr!("source_control.order_cli", tool = tool).into_owned()
        }
    }
}

fn status_line(status: Option<&HostStatus>) -> String {
    match status.and_then(|status| status.source.as_ref()) {
        Some(CredentialSource::Saved) => crate::tr!("source_control.using_saved").into_owned(),
        Some(CredentialSource::SavedBasic { email }) => {
            crate::tr!("source_control.using_saved_basic", email = email).into_owned()
        }
        Some(CredentialSource::SavedBearer) => {
            crate::tr!("source_control.using_saved_bearer").into_owned()
        }
        Some(CredentialSource::Env { name }) => {
            crate::tr!("source_control.using_env", name = name).into_owned()
        }
        Some(CredentialSource::Cli { tool }) => {
            crate::tr!("source_control.using_cli", tool = tool).into_owned()
        }
        None => crate::tr!("source_control.not_connected").into_owned(),
    }
}

pub struct SourceControlPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<crate::window_state::WindowState>,
    tokens: BTreeMap<String, Entity<InputState>>,
    /// The account email beside the token, for hosts whose tokens may take one.
    emails: BTreeMap<String, Entity<InputState>>,
    /// Whether the token being typed is one sent with the email, for those hosts.
    with_email: BTreeMap<String, bool>,
    visible: bool,
    _subscription: Subscription,
}

impl SourceControlPanel {
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<crate::window_state::WindowState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&store, |_, _, change: &StoreChange, cx| {
            if change.topic == TopicKind::Settings {
                cx.notify();
            }
        });
        Self {
            store,
            window_state,
            tokens: BTreeMap::new(),
            emails: BTreeMap::new(),
            with_email: BTreeMap::new(),
            visible: false,
            _subscription: subscription,
        }
    }
    pub fn show(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.visible = true;
            self.store
                .update(cx, |store, _| store.refresh_host_credentials());
        }
    }
    pub fn hide(&mut self) {
        self.visible = false;
        self.tokens.clear();
        self.emails.clear();
        self.with_email.clear();
    }

    fn notice(&self, host: &str, problem: &HostProblem, cx: &App) -> Option<AnyElement> {
        let theme = cx.theme();
        let text = match problem {
            HostProblem::NotSignedIn { tool, .. } => {
                crate::tr!(
                    "source_control.notice_not_signed_in",
                    tool = tool,
                    host = host
                )
            }
            HostProblem::NoCredential { .. } | HostProblem::Unverified => return None,
        };
        let command = match problem {
            HostProblem::NotSignedIn {
                command: Some(command),
                ..
            } => Some(command.clone()),
            _ => None,
        };
        let compact = self.window_state.read(cx).compact;
        let selector = format!("source-control-notice-{host}");
        Some(
            crate::material::card(cx)
                .w_full()
                .debug_selector(move || selector)
                .text_token(&theme.tokens.typography.xs)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(crate::icon::IconName::TriangleAlert)
                                .text_color(theme.warning),
                        )
                        .child(div().flex_1().min_w_0().child(text.into_owned())),
                )
                .when_some(command, |notice, command| {
                    let copied = command.clone();
                    notice.child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            // Wraps between words only, so a URL in the command stays whole.
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .flex_wrap()
                                    .px_1()
                                    .rounded(crate::material::radius_chip(cx))
                                    .bg(theme.secondary)
                                    .font_family(theme.mono_font_family.clone())
                                    // Each word keeps the space after it, so the gaps are the
                                    // font's own and a line breaks only after one.
                                    .children(command.split_inclusive(' ').map(|word| {
                                        div().whitespace_nowrap().child(word.to_owned())
                                    })),
                            )
                            .child(crate::widgets::copy::copy_button(
                                &format!("source-control-command-{host}"),
                                false,
                                compact,
                                move |_, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        copied.clone(),
                                    ))
                                },
                                cx,
                            )),
                    )
                })
                .into_any_element(),
        )
    }

    fn row(
        &mut self,
        host: &str,
        kind: HostKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = self.store.read(cx).settings().source_control.clone();
        let choice = settings
            .hosts
            .get(host)
            .cloned()
            .unwrap_or_else(|| HostSettings::new(kind));
        let status = settings.status.get(host).cloned();
        let theme = cx.theme().clone();
        let compact = self.window_state.read(cx).compact;
        let store = self.store.clone();
        let toggle_host = host.to_owned();
        let switch = Switch::new(SharedString::from(format!("source-control-enabled-{host}")))
            .checked(choice.enabled)
            .aria_label(crate::tr!("source_control.use_host", host = host).into_owned())
            .tooltip(crate::tr!("source_control.use_host", host = host).into_owned())
            .on_click(move |enabled, _, cx| {
                store.update(cx, |store, cx| {
                    store.patch_source_control_host(
                        toggle_host.clone(),
                        kind,
                        Some(*enabled),
                        None,
                    );
                    cx.notify();
                });
            });
        let added = status
            .as_ref()
            .map_or(settings.hosts.contains_key(host), |status| status.added)
            && host != kind.public_host();
        let mut kind_line = kind.terms().name.to_owned();
        if added {
            kind_line.push_str(" · ");
            kind_line.push_str(&crate::tr!("source_control.origin_added"));
        }
        let header_color = if choice.enabled {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        let header = h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .text_body(compact, cx)
            .child(mark(kind).text_color(header_color))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_color(header_color)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(host.to_owned())),
                    )
                    .child(div().text_secondary(compact, cx).child(kind_line)),
            )
            .child(switch);
        let mut row = crate::material::group_row_stacked(cx).child(header);
        if !choice.enabled {
            return row
                .child(
                    div()
                        .text_secondary(compact, cx)
                        .child(crate::tr!("source_control.off", host = host).into_owned()),
                )
                .into_any_element();
        }
        let order = status
            .as_ref()
            .map(|status| {
                status
                    .order
                    .iter()
                    .map(source_slot)
                    .collect::<Vec<_>>()
                    .join(" → ")
            })
            .unwrap_or_default();
        let tooltip = crate::tr!("source_control.order", order = order).into_owned();
        row = row.child(
            div()
                .id(SharedString::from(format!("source-control-source-{host}")))
                .flex()
                .flex_wrap()
                .gap_1()
                .text_secondary(compact, cx)
                .child(status_line(status.as_ref()))
                .when(
                    status
                        .as_ref()
                        .is_some_and(|status| status.problem == Some(HostProblem::Unverified)),
                    |line| {
                        let selector = format!("source-control-unverified-{host}");
                        line.child(" · ").child(
                            div()
                                .debug_selector(move || selector)
                                .child(crate::tr!("source_control.unverified").into_owned()),
                        )
                    },
                )
                .when(!order.is_empty(), |line| {
                    line.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                }),
        );
        if let Some(problem) = status.as_ref().and_then(|status| status.problem.as_ref()) {
            row = row.children(self.notice(host, problem, cx));
        }
        let accounts = status
            .as_ref()
            .map(|status| status.accounts.clone())
            .unwrap_or_default();
        if accounts.len() > 1 {
            let selected = choice
                .account
                .clone()
                .unwrap_or_else(|| crate::tr!("source_control.active_account").into_owned());
            let selected_account = choice.account.clone();
            let menu_host = host.to_owned();
            row = row.child(
                Button::new(SharedString::from(format!("source-control-account-{host}")))
                    .ghost()
                    .outline()
                    .compact()
                    .label(selected)
                    .dropdown_menu(move |mut menu, _, _| {
                        menu = menu.menu_with_check(
                            crate::tr!("source_control.active_account").into_owned(),
                            selected_account.is_none(),
                            Box::new(ChooseAccount {
                                host: menu_host.clone(),
                                kind,
                                account: None,
                            }),
                        );
                        for account in &accounts {
                            menu = menu.menu_with_check(
                                account.clone(),
                                selected_account.as_ref() == Some(account),
                                Box::new(ChooseAccount {
                                    host: menu_host.clone(),
                                    kind,
                                    account: Some(account.clone()),
                                }),
                            );
                        }
                        menu
                    }),
            );
        }
        if status
            .as_ref()
            .is_some_and(|status| status.env_overrides_account)
        {
            row = row.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("source_control.env_override").into_owned()),
            );
        }
        if kind.takes_email() {
            return row
                .child(self.email_editor(host, status.as_ref(), window, cx))
                .into_any_element();
        }
        let token_set = status.as_ref().is_some_and(|status| status.token_set);
        let placeholder = if token_set {
            crate::tr!("source_control.token_placeholder_saved")
        } else {
            crate::tr!("source_control.token_placeholder")
        };
        let token = self
            .tokens
            .entry(host.to_owned())
            .or_insert_with(|| cx.new(|cx| InputState::new(window, cx).masked(true)))
            .clone();
        let set_host = host.to_owned();
        let clear_host = host.to_owned();
        let input = token.clone();
        let store = self.store.clone();
        let clear_store = self.store.clone();
        let field =
            div()
                // The token field's floor before the buttons wrap; gpui-base has no field-width token.
                .min_w(gpui::px(120.))
                .map(|field| {
                    if compact {
                        field.w_full()
                    } else {
                        field.flex_1()
                    }
                })
                .child(Input::new(&token).placeholder(placeholder).aria_label(
                    crate::tr!("source_control.token_label", host = host).into_owned(),
                ));
        let mut buttons = h_flex()
            .gap_2()
            .child(
                Button::new(SharedString::from(format!(
                    "source-control-token-set-{host}"
                )))
                .ghost()
                .outline()
                .compact()
                .touch(compact)
                .label(crate::tr!("source_control.set").into_owned())
                .aria_label(crate::tr!("source_control.set_label", host = host).into_owned())
                .on_click(move |_, window, cx| {
                    let value = input.read(cx).value().to_string();
                    if !value.trim().is_empty() {
                        store.update(cx, |store, _| {
                            store.set_host_token(set_host.clone(), Some(value), None)
                        });
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                    }
                }),
            )
            .child(
                Button::new(SharedString::from(format!(
                    "source-control-token-clear-{host}"
                )))
                .ghost()
                .compact()
                .touch(compact)
                .disabled(!token_set)
                .label(crate::tr!("source_control.clear").into_owned())
                .aria_label(crate::tr!("source_control.clear_label", host = host).into_owned())
                .on_click(move |_, _, cx| {
                    clear_store.update(cx, |store, _| {
                        store.set_host_token(clear_host.clone(), None, None)
                    })
                }),
            );
        if added {
            let remove_host = host.to_owned();
            let store = self.store.clone();
            buttons = buttons.child(
                Button::new(SharedString::from(format!("source-control-remove-{host}")))
                    .ghost()
                    .compact()
                    .touch(compact)
                    .label(crate::tr!("source_control.remove_host").into_owned())
                    .on_click(move |_, window, cx| {
                        confirm_remove(store.clone(), remove_host.clone(), window, cx)
                    }),
            );
        }
        // A phone gives the field the row and puts the buttons under it, at the end.
        let editor = if compact {
            v_flex()
                .w_full()
                .gap_2()
                .child(field)
                .child(h_flex().w_full().justify_end().child(buttons))
        } else {
            h_flex().w_full().gap_2().child(field).child(buttons)
        };
        let scopes = status
            .as_ref()
            .filter(|status| !status.token_scopes.is_empty())
            .map(|status| {
                crate::tr!(
                    "source_control.token_scopes",
                    scopes = status.token_scopes.join(", ")
                )
                .into_owned()
            });
        let token_url = status.and_then(|status| status.token_url);
        let help = (scopes.is_some() || token_url.is_some()).then(|| {
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .items_center()
                .text_secondary(compact, cx)
                .children(scopes)
                .when_some(token_url, |help, url| {
                    help.child(crate::settings_rows::external_link(
                        format!("source-control-token-link-{host}"),
                        crate::tr!("source_control.create_token")
                            .into_owned()
                            .into(),
                        url,
                    ))
                })
        });
        row.child(editor).children(help).into_any_element()
    }
}

impl SourceControlPanel {
    /// The editor of a host whose token is either an API token sent with the account's email
    /// or an access token sent alone: one is chosen, and saving it replaces the other.
    fn email_editor(
        &mut self,
        host: &str,
        status: Option<&HostStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let saved = status.and_then(|status| status.source.clone());
        let saved_email = match &saved {
            Some(CredentialSource::SavedBasic { email }) => Some(email.clone()),
            _ => None,
        };
        let bearer_saved = saved == Some(CredentialSource::SavedBearer);
        let with_email = *self
            .with_email
            .entry(host.to_owned())
            .or_insert(!bearer_saved);
        let token_saved = if with_email {
            saved_email.is_some()
        } else {
            bearer_saved
        };
        let token = self
            .tokens
            .entry(host.to_owned())
            .or_insert_with(|| cx.new(|cx| InputState::new(window, cx).masked(true)))
            .clone();
        token.update(cx, |input, cx| {
            input.set_placeholder(
                if token_saved {
                    crate::tr!("source_control.token_placeholder_saved")
                } else {
                    crate::tr!("source_control.token_placeholder")
                }
                .into_owned(),
                window,
                cx,
            )
        });
        let email = match self.emails.get(host) {
            Some(email) => email.clone(),
            None => {
                let email = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(crate::tr!("source_control.bitbucket_email"))
                });
                if let Some(saved_email) = &saved_email {
                    email.update(cx, |input, cx| input.set_value(saved_email, window, cx));
                }
                self.emails.insert(host.to_owned(), email.clone());
                email
            }
        };
        let panel = cx.entity().downgrade();
        let method = |id: &str, label: SharedString, chosen: bool| {
            let panel = panel.clone();
            let host = host.to_owned();
            Choice::new(
                SharedString::from(format!("source-control-method-{id}-{host}")),
                label,
            )
            .selected(with_email == chosen)
            .on_select(move |_, cx| {
                let _ = panel.update(cx, |panel, cx| {
                    panel.with_email.insert(host.clone(), chosen);
                    cx.notify();
                });
            })
        };
        let track = ChoiceGroup::new(SharedString::from(format!("source-control-method-{host}")))
            .compact(compact)
            .choices([
                method(
                    "api",
                    crate::tr!("source_control.bitbucket_api_token")
                        .into_owned()
                        .into(),
                    true,
                ),
                method(
                    "access",
                    crate::tr!("source_control.bitbucket_access_token")
                        .into_owned()
                        .into(),
                    false,
                ),
            ]);
        let (description, link_label, link) = if with_email {
            (
                crate::tr!("source_control.bitbucket_api_desc"),
                crate::tr!("source_control.bitbucket_api_link"),
                status.and_then(|status| status.token_url.clone()),
            )
        } else {
            (
                crate::tr!("source_control.bitbucket_access_desc"),
                crate::tr!("source_control.bitbucket_access_link"),
                status.and_then(|status| status.access_token_url.clone()),
            )
        };
        let description = v_flex()
            .w_full()
            .items_start()
            .gap_1()
            .text_secondary(compact, cx)
            .child(div().w_full().child(description.into_owned()))
            .when_some(link, |description, url| {
                description.child(crate::settings_rows::external_link(
                    format!("source-control-method-link-{host}"),
                    link_label.into_owned().into(),
                    url,
                ))
            });
        let replaces = match (with_email, &saved) {
            (true, Some(CredentialSource::SavedBearer)) => {
                Some(crate::tr!("source_control.bitbucket_replaces_access"))
            }
            (false, Some(CredentialSource::SavedBasic { .. })) => {
                Some(crate::tr!("source_control.bitbucket_replaces_api"))
            }
            _ => None,
        };
        let store = self.store.clone();
        let set_host = host.to_owned();
        let (set_token, set_email) = (token.clone(), email.clone());
        let set = Button::new(SharedString::from(format!(
            "source-control-token-set-{host}"
        )))
        .ghost()
        .outline()
        .compact()
        .touch(compact)
        .label(crate::tr!("source_control.set").into_owned())
        .aria_label(crate::tr!("source_control.set_label", host = host).into_owned())
        .on_click(move |_, window, cx| {
            let token = set_token.read(cx).value().trim().to_owned();
            let token = (!token.is_empty()).then_some(token);
            let email = with_email
                .then(|| set_email.read(cx).value().trim().to_owned())
                .filter(|email| !email.is_empty());
            // An API token needs its email, and either a new token or the one saved with
            // an email; an access token needs a new token.
            let ready = match (with_email, &email, &token) {
                (true, None, _) => false,
                (true, Some(_), token) => token.is_some() || token_saved,
                (false, _, token) => token.is_some(),
            };
            if ready {
                store.update(cx, |store, _| {
                    store.set_host_token(set_host.clone(), token, email)
                });
                set_token.update(cx, |input, cx| input.set_value("", window, cx));
            }
        });
        let clear_store = self.store.clone();
        let clear_host = host.to_owned();
        let clear = Button::new(SharedString::from(format!(
            "source-control-token-clear-{host}"
        )))
        .ghost()
        .compact()
        .touch(compact)
        .disabled(!status.is_some_and(|status| status.token_set))
        .label(crate::tr!("source_control.clear").into_owned())
        .aria_label(crate::tr!("source_control.clear_label", host = host).into_owned())
        .on_click(move |_, _, cx| {
            clear_store.update(cx, |store, _| {
                store.set_host_token(clear_host.clone(), None, None)
            })
        });
        let email_label = crate::tr!("source_control.bitbucket_email_label", host = host);
        v_flex()
            .w_full()
            .gap_2()
            .child(track)
            .child(description)
            .when(with_email, |editor| {
                editor.child(
                    div()
                        .w_full()
                        .child(Input::new(&email).aria_label(email_label.into_owned())),
                )
            })
            .child(
                div().w_full().child(Input::new(&token).aria_label(
                    crate::tr!("source_control.token_label", host = host).into_owned(),
                )),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(compact, cx)
                            .children(replaces.map(|replaces| replaces.into_owned())),
                    )
                    .child(h_flex().gap_2().child(set).child(clear)),
            )
            .into_any_element()
    }
}

fn confirm_remove(store: Entity<WorkspaceStore>, host: String, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let store = store.clone();
        let host = host.clone();
        alert
            .title(crate::tr!("source_control.remove_title", host = &host).into_owned())
            .description(crate::tr!("source_control.remove_desc").into_owned())
            .button_props(
                crate::overlay::DialogButtons::default()
                    .ok_variant(ButtonVariant::Danger)
                    .ok_text(crate::tr!("source_control.remove_host"))
                    .cancel_text(crate::tr!("settings.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                store.update(cx, |store, _| {
                    store.remove_source_control_host(host.clone())
                });
                true
            })
    });
}

impl Render for SourceControlPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.store.read(cx).settings().source_control.clone();
        let hosts = listed(&settings);
        let rows = hosts
            .iter()
            .map(|(host, kind)| self.row(host, *kind, window, cx))
            .collect();
        let compact = self.window_state.read(cx).compact;
        let store = self.store.clone();
        let listed_hosts: Vec<String> = hosts.into_iter().map(|(host, _)| host).collect();
        let add = Button::new("source-control-add-host")
            .outline()
            .small()
            .icon(crate::icon::IconName::Plus)
            .when(compact, |button| button.w_full())
            .label(crate::tr!("source_control.add_host_menu").into_owned())
            .on_click(move |_, window, cx| {
                AddHostDialog::open(store.clone(), listed_hosts.clone(), window, cx)
            });
        let note = div()
            .text_secondary(compact, cx)
            .child(crate::tr!("source_control.detection_note").into_owned());
        v_flex()
            .gap(crate::material::section_gap(cx))
            .on_action(cx.listener(|this, action: &ChooseAccount, _, cx| {
                this.store.update(cx, |store, _| {
                    store.patch_source_control_host(
                        action.host.clone(),
                        action.kind,
                        None,
                        Some(action.account.clone()),
                    )
                });
            }))
            .child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("source_control.description").into_owned()),
            )
            .child(
                v_flex()
                    .child(crate::material::group_caption(
                        crate::tr!("settings.hosts_section"),
                        cx,
                    ))
                    .child(crate::material::grouped(rows, cx)),
            )
            .child(if compact {
                v_flex().gap_2().child(add).child(note)
            } else {
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .child(note.flex_1())
                    .child(add)
            })
    }
}

/// The Add host dialog: a kind and an authority, checked by the kind's own rule before the host
/// is asked to add it.
struct AddHostDialog {
    store: Entity<WorkspaceStore>,
    listed: Vec<String>,
    kind: HostKind,
    input: Entity<InputState>,
    error: Option<String>,
    _subscription: Subscription,
}

impl AddHostDialog {
    fn open(store: Entity<WorkspaceStore>, listed: Vec<String>, window: &mut Window, cx: &mut App) {
        let dialog = cx.new(|cx| {
            let kind = HostKind::Github;
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::tr!(&format!(
                    "source_control.host_placeholder_{}",
                    slug(kind)
                )))
            });
            input.update(cx, |input, cx| {
                input.set_value(prefill(kind), window, cx);
                input.focus(window, cx);
            });
            let subscription = cx.subscribe_in(
                &input,
                window,
                |dialog: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        dialog.error = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => dialog.add(window, cx),
                    _ => {}
                },
            );
            Self {
                store,
                listed,
                kind,
                input,
                error: None,
                _subscription: subscription,
            }
        });
        let content = dialog.clone();
        let footer = dialog.clone();
        window.open_dialog(cx, move |builder, _, _| {
            let content = content.clone();
            let cancel = footer.clone();
            let add = footer.clone();
            builder
                .title(crate::tr!("source_control.add_host").into_owned())
                .content(move |el, _, _| el.child(content.clone()))
                .footer(
                    DialogActions::new()
                        .child(
                            Button::new("source-control-add-cancel")
                                .outline()
                                .small()
                                .label(crate::tr!("settings.cancel"))
                                .on_click(move |_, window, cx| {
                                    let _ = &cancel;
                                    window.close_dialog(cx)
                                }),
                        )
                        .child(
                            Button::new("source-control-add-confirm")
                                .primary()
                                .small()
                                .label(crate::tr!("source_control.add"))
                                .on_click(move |_, window, cx| {
                                    add.update(cx, |dialog, cx| dialog.add(window, cx))
                                }),
                        )
                        .into_any_element(),
                )
        });
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().to_string();
        let name = self.kind.terms().name;
        let host = match self.kind.authority(&typed) {
            Ok(host) => host,
            Err(HostRefusal::Blank) if self.kind.organizations() => {
                self.error =
                    Some(crate::tr!("source_control.error_blank_organization").into_owned());
                return cx.notify();
            }
            Err(HostRefusal::Invalid) if self.kind.organizations() => {
                self.error =
                    Some(crate::tr!("source_control.error_invalid_organization").into_owned());
                return cx.notify();
            }
            Err(HostRefusal::Blank) => {
                self.error = Some(crate::tr!("source_control.error_blank").into_owned());
                return cx.notify();
            }
            Err(HostRefusal::Invalid) => {
                self.error = Some(crate::tr!("source_control.error_invalid").into_owned());
                return cx.notify();
            }
            Err(HostRefusal::PortOrPath) => {
                self.error =
                    Some(crate::tr!("source_control.error_port_path", kind = name).into_owned());
                return cx.notify();
            }
            Err(HostRefusal::Path) => {
                self.error =
                    Some(crate::tr!("source_control.error_path", kind = name).into_owned());
                return cx.notify();
            }
            Err(HostRefusal::Fixed) => {
                self.error = Some(
                    crate::tr!(
                        "source_control.error_fixed",
                        kind = name,
                        host = self.kind.public_host()
                    )
                    .into_owned(),
                );
                return cx.notify();
            }
        };
        if self.listed.contains(&host) {
            self.error = Some(crate::tr!("source_control.error_exists", host = &host).into_owned());
            return cx.notify();
        }
        let kind = self.kind;
        self.store.update(cx, |store, _| {
            store.patch_source_control_host(host, kind, Some(true), None)
        });
        window.close_dialog(cx);
    }

    fn choose(&mut self, kind: HostKind, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().trim().to_ascii_lowercase();
        let previous = self.kind;
        self.kind = kind;
        self.error = None;
        self.input.update(cx, |input, cx| {
            input.set_placeholder(
                crate::tr!(&format!("source_control.host_placeholder_{}", slug(kind))).into_owned(),
                window,
                cx,
            );
            if typed.is_empty() || typed == previous.public_host() || kind.fixed_host().is_some() {
                input.set_value(prefill(kind), window, cx);
            }
        });
        cx.notify();
    }
}

impl Render for AddHostDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let theme = cx.theme();
        let kind = self.kind;
        let label = |text: SharedString| div().text_caption(cx).child(text);
        v_flex()
            .gap_2()
            .on_action(cx.listener(|this, action: &ChooseKind, window, cx| {
                this.choose(action.kind, window, cx)
            }))
            .child(label(
                crate::tr!("source_control.kind_label").into_owned().into(),
            ))
            .child(
                Button::new("source-control-add-kind")
                    .outline()
                    .small()
                    .w_full()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(mark(kind))
                            .child(kind.terms().name)
                            .child(
                                div()
                                    .ml_auto()
                                    .child(Icon::new(crate::icon::IconName::ChevronDown)),
                            ),
                    )
                    // gpui-base has no Select; a button with a menu stands in for one.
                    .dropdown_menu(move |mut menu, _, _| {
                        for choice in HostKind::ALL {
                            menu = menu.menu_with_check(
                                choice.terms().name,
                                choice == kind,
                                Box::new(ChooseKind { kind: choice }),
                            );
                        }
                        menu
                    }),
            )
            .child(label(
                if kind.organizations() {
                    crate::tr!("source_control.organization_label")
                } else {
                    crate::tr!("source_control.host_label")
                }
                .into_owned()
                .into(),
            ))
            // A kind served from one host alone takes no other.
            .child(Input::new(&self.input).disabled(kind.fixed_host().is_some()))
            .child(
                div()
                    .text_secondary(compact, cx)
                    .flex()
                    .flex_wrap()
                    .children(
                        crate::tr!(&format!("source_control.note_{}", slug(kind)))
                            .split_inclusive([' ', '。'])
                            .map(|word| {
                                div()
                                    .min_w_0()
                                    .when(word.is_ascii() || word.contains('.'), |fragment| {
                                        fragment.whitespace_nowrap()
                                    })
                                    .child(word.to_owned())
                            }),
                    ),
            )
            .child(
                div()
                    // Holds one line while empty, so an error does not move the dialog.
                    .min_h(theme.tokens.typography.xs.line_height)
                    .text_token(&theme.tokens.typography.xs)
                    .text_color(theme.danger)
                    .children(self.error.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic, encode_line};

    #[gpui::test]
    fn credential_rows_show_only_actionable_notices(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let status = |kind, source, problem| HostStatus {
            kind,
            added: true,
            token_set: true,
            source: Some(source),
            accounts: Vec::new(),
            env_overrides_account: false,
            order: vec![CredentialSource::Saved],
            problem,
            token_url: None,
            access_token_url: None,
            token_scopes: Vec::new(),
        };
        let mut settings = tcode_core::settings::Settings {
            language: Some(crate::LANGUAGE_ENGLISH.into()),
            ..Default::default()
        };
        settings.source_control.status = BTreeMap::from([
            (
                "bitbucket.org".to_owned(),
                status(
                    HostKind::Bitbucket,
                    CredentialSource::SavedBearer,
                    Some(HostProblem::Unverified),
                ),
            ),
            (
                "gitlab.com".to_owned(),
                status(HostKind::Gitlab, CredentialSource::Saved, None),
            ),
        ]);
        let mut empty = status(
            HostKind::Gitea,
            CredentialSource::Saved,
            Some(HostProblem::NoCredential {
                tools_missing: vec!["tea".into()],
            }),
        );
        empty.source = None;
        empty.token_set = false;
        settings
            .source_control
            .status
            .insert("gitea.com".into(), empty.clone());
        empty.kind = HostKind::Gitlab;
        empty.problem = Some(HostProblem::NotSignedIn {
            tool: "glab".into(),
            command: Some("glab auth login --hostname gitlab.example.com".into()),
        });
        settings
            .source_control
            .status
            .insert("gitlab.example.com".into(), empty);
        let (to_host, _requests) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(settings),
                }))
                .unwrap(),
            )
            .unwrap();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump.pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| crate::window_state::WindowState::new(false));
        let (_, cx) =
            cx.add_window_view(|_, cx| SourceControlPanel::new(store.clone(), window_state, cx));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(
            cx.debug_bounds("source-control-unverified-bitbucket.org")
                .is_some()
        );
        assert!(
            cx.debug_bounds("source-control-notice-bitbucket.org")
                .is_none()
        );
        assert!(cx.debug_bounds("source-control-notice-gitea.com").is_none());
        assert!(
            cx.debug_bounds("source-control-notice-gitlab.example.com")
                .is_some()
        );
        assert!(
            cx.debug_bounds("source-control-notice-gitlab.com")
                .is_none()
        );
    }
}

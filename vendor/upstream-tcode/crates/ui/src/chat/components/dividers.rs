use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use agent::ProviderKind;
use gpui::{
    AnyElement, App, Div, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, Styled as _, div, px,
};
use gpui_base::h_flex;

/// Width of the hairline stub flanking a divider label. Short on purpose: a
/// full-width rule would cut the flow in two, and these events only annotate it.
const STUB_WIDTH: f32 = 24.;

/// One hairline stub of the divider grammar, shared with the centered
/// disclosure rows so every ambient notification flanks its label identically.
pub(crate) fn divider_stub(cx: &App) -> Div {
    div().h(px(1.)).w(px(STUB_WIDTH)).bg(cx.theme().border)
}

/// The one divider grammar: a centered small label between two hairline stubs.
/// `tint` colors the label alone — the rules stay border-quiet no matter what
/// the event is, so no divider can shout louder than another.
fn divider(id: SharedString, label: String, tint: Hsla, cx: &App) -> AnyElement {
    let stub = || divider_stub(cx);
    h_flex()
        .id(id)
        .w_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(stub())
        .child(
            div()
                .min_w_0()
                .flex_shrink_1()
                .text_center()
                .text_token(&cx.theme().tokens.typography.xs)
                .text_color(tint)
                .child(label),
        )
        .child(stub())
        .into_any_element()
}

pub(crate) fn relay_divider(
    id: &str,
    from: ProviderKind,
    to: ProviderKind,
    moved_from: Option<&std::path::Path>,
    cx: &App,
) -> AnyElement {
    let label = match moved_from {
        Some(dir) => crate::tr!(
            "chat.moved",
            from = dir.to_string_lossy(),
            to = to.display_name()
        ),
        None => crate::tr!(
            "chat.relayed",
            from = from.display_name(),
            to = to.display_name()
        ),
    };
    divider(
        SharedString::from(format!("relay-{id}")),
        label.into_owned(),
        cx.theme().muted_foreground,
        cx,
    )
}

pub(crate) fn model_change_divider(
    id: &str,
    from: Option<&str>,
    to: &str,
    reason: Option<&str>,
    cx: &App,
) -> AnyElement {
    let label = match from {
        Some(from) => crate::tr!("chat.model_changed", from = from, to = to).into_owned(),
        None => crate::tr!("chat.model_changed_to", to = to).into_owned(),
    };
    let label = match reason {
        Some(reason) if !reason.is_empty() => format!("{label} ({reason})"),
        _ => label,
    };
    // A model swap is worth noticing, so the text carries a warning tint — but
    // nothing else about the row differs from a relay.
    divider(
        SharedString::from(format!("model-change-{id}")),
        label,
        cx.theme().warning,
        cx,
    )
}

/// Context compaction rewrites what the model remembers, so it announces
/// itself at model-swap prominence rather than hiding in the work log.
pub(crate) fn context_compacted_divider(
    id: &str,
    metadata: Option<&agent::Compaction>,
    cx: &App,
) -> AnyElement {
    let mut label = if metadata.is_some_and(|c| c.in_progress) {
        crate::tr!("chat.context_compacting").into_owned()
    } else {
        crate::tr!("chat.context_compacted").into_owned()
    };
    if let Some(c) = metadata {
        if let Some(trigger) = &c.trigger {
            let trigger = match trigger.as_str() {
                "manual" => crate::tr!("chat.context_manual").into_owned(),
                "auto" => crate::tr!("chat.context_auto").into_owned(),
                other => other.to_owned(),
            };
            label.push_str(&format!(" · {trigger}"));
        }
        if let Some(tokens) = c.pre_tokens {
            label.push_str(&format!(
                " · {}",
                crate::tr!(
                    "chat.context_pre_tokens",
                    tokens = crate::context_meter::format_tokens(Some(tokens))
                )
            ));
        }
    }
    divider(
        SharedString::from(format!("context-compacted-{id}")),
        label,
        cx.theme().warning,
        cx,
    )
}

pub(crate) fn control_action_divider(
    id: &str,
    action: &agent::ControlAction,
    cx: &App,
) -> AnyElement {
    let mut label = crate::tr!(
        "chat.control_action",
        actor = action.actor_title.as_str(),
        action = crate::control::action_label(&action.action)
    )
    .into_owned();
    if let Some(detail) = action.detail.as_deref().filter(|detail| !detail.is_empty()) {
        label.push_str(&format!(" ({detail})"));
    }
    divider(
        SharedString::from(format!("control-action-{id}")),
        label,
        cx.theme().muted_foreground,
        cx,
    )
}

pub(crate) fn context_window_changed_divider(id: &str, window: u64, cx: &App) -> AnyElement {
    divider(
        SharedString::from(format!("context-window-changed-{id}")),
        crate::tr!(
            "chat.context_window_changed",
            window = agent::context_window::format(window)
        )
        .into_owned(),
        cx.theme().muted_foreground,
        cx,
    )
}

//! The copy affordance every surface shares: a ghost icon button that shows a
//! check for two seconds after it fired, and the per-entity record of which
//! button that was.

use std::time::Duration;

use gpui::{App, ClickEvent, Context, SharedString, Styled as _, Task, Window};

use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    widgets::button::{Button, ButtonVariants as _},
};

pub(crate) fn action_button(
    id: impl Into<SharedString>,
    icon: IconName,
    label: impl Into<SharedString>,
    cx: &App,
) -> Button {
    Button::new(id.into())
        .ghost()
        .small()
        .icon(Icon::new(icon).text_color(cx.theme().muted_foreground))
        .tooltip(label.into())
}

pub(crate) fn copy_button(
    key: &str,
    copied: bool,
    compact: bool,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Button {
    action_button(
        SharedString::from(format!("copy-{key}")),
        if copied {
            IconName::Check
        } else {
            IconName::Copy
        },
        if copied {
            crate::tr!("chat.copied").into_owned()
        } else {
            crate::tr!("chat.copy").into_owned()
        },
        cx,
    )
    .touch(compact)
    .on_click(on_copy)
}

/// Which copy button of an entity is showing its confirmation, by the key the
/// button was built with; it clears itself two seconds after the copy.
#[derive(Default)]
pub(crate) struct CopiedMark {
    key: Option<String>,
    _reset: Option<Task<()>>,
}

impl CopiedMark {
    pub(crate) fn is(&self, key: &str) -> bool {
        self.key.as_deref() == Some(key)
    }

    /// `slot` reaches this mark inside `T` again when the reset fires.
    pub(crate) fn mark<T: 'static>(
        &mut self,
        key: String,
        slot: impl Fn(&mut T) -> &mut CopiedMark + 'static,
        cx: &mut Context<T>,
    ) {
        self.key = Some(key.clone());
        self._reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                let mark = slot(this);
                if mark.key.as_deref() == Some(key.as_str()) {
                    mark.key = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

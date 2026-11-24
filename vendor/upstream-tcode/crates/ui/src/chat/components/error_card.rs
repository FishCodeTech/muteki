use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, ClickEvent, IntoElement as _, ParentElement as _, SharedString, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

pub(crate) enum LimitResume {
    Retry {
        remaining_secs: Option<u64>,
        on_retry: ClickHandler,
        on_cancel: ClickHandler,
    },
    /// No resume is queued and the reset is still ahead: offer to schedule one.
    Offer { on_schedule: ClickHandler },
    /// A resume is queued for the reset: live countdown plus cancel.
    Scheduled {
        remaining_secs: u64,
        on_cancel: ClickHandler,
    },
    /// The reset has passed and nothing followed the error (the scheduled
    /// resume does not survive a restart): offer to continue right now.
    Continue { on_continue: ClickHandler },
}

/// A provider/app error as a first-class timeline block: a danger-tinted card
/// carrying the full message, wrapped across as many lines as it needs.
pub(crate) fn error_card(
    id: &str,
    message: &str,
    copied: bool,
    resume: Option<LimitResume>,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let danger = cx.theme().danger;
    let copy = Button::new(SharedString::from(format!("copy-error:{id}")))
        .ghost()
        .xsmall()
        .icon(if copied {
            IconName::Check
        } else {
            IconName::Copy
        })
        .label(if copied {
            crate::tr!("chat.copied")
        } else {
            crate::tr!("chat.copy")
        })
        .on_click(on_copy);
    let resume_row = resume.map(|resume| match resume {
        LimitResume::Retry {
            remaining_secs,
            on_retry,
            on_cancel,
        } => h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new(SharedString::from(format!("retry-turn:{id}")))
                    .outline()
                    .xsmall()
                    .label(match remaining_secs {
                        Some(seconds) => {
                            crate::tr!("chat.capacity_retry.countdown", seconds = seconds)
                        }
                        None => crate::tr!("chat.capacity_retry.retry"),
                    })
                    .on_click(on_retry),
            )
            .when(remaining_secs.is_some(), |row| {
                row.child(
                    Button::new(SharedString::from(format!("cancel-retry:{id}")))
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("chat.limit_resume.cancel"))
                        .on_click(on_cancel),
                )
            })
            .into_any_element(),
        LimitResume::Offer { on_schedule } => h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new(SharedString::from(format!("schedule-limit-resume:{id}")))
                    .outline()
                    .xsmall()
                    .icon(IconName::ArrowUp)
                    .label(crate::tr!("chat.limit_resume.schedule"))
                    .on_click(on_schedule),
            )
            .into_any_element(),
        LimitResume::Continue { on_continue } => h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new(SharedString::from(format!("continue-limit-resume:{id}")))
                    .outline()
                    .xsmall()
                    .icon(IconName::ArrowUp)
                    .label(crate::tr!("chat.limit_resume.continue_now"))
                    .on_click(on_continue),
            )
            .into_any_element(),
        LimitResume::Scheduled {
            remaining_secs,
            on_cancel,
        } => h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_secondary(false, cx)
                    .text_color(cx.theme().danger_foreground)
                    .child(crate::tr!(
                        "chat.limit_resume.resumes_in",
                        countdown = crate::time::format_countdown(remaining_secs)
                    )),
            )
            .child(div().flex_1())
            .child(
                Button::new(SharedString::from(format!("cancel-limit-resume:{id}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .label(crate::tr!("chat.limit_resume.cancel"))
                    .on_click(on_cancel),
            )
            .into_any_element(),
    });
    let content = v_flex()
        .flex_1()
        .min_w_0()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .text_token(&cx.theme().tokens.typography.xs)
                .child(Icon::new(IconName::TriangleAlert).text_color(cx.theme().danger_foreground))
                .child(
                    div()
                        .text_size(px(10.5))
                        .font_medium()
                        .text_color(cx.theme().danger_foreground)
                        .child(crate::material::tracked_uppercase(
                            crate::tr!("chat.error_label").as_ref(),
                        )),
                )
                .child(div().flex_1())
                .child(copy),
        )
        .child(
            div()
                .w_full()
                .text_body(false, cx)
                .text_color(cx.theme().danger_foreground)
                .whitespace_normal()
                .child(message.to_string()),
        )
        .when_some(resume_row, |content, row| content.child(row));
    crate::material::notice(danger, cx)
        .items_stretch()
        .overflow_hidden()
        // The danger rail is a hairline accent, not a spacing step.
        .child(div().flex_none().w(px(2.)).rounded_full().bg(danger))
        .child(content)
        .context_menu({
            let message = message.to_string();
            move |menu, _, _| {
                menu.menu(
                    crate::tr!("chat.copy_error").into_owned(),
                    Box::new(CopyText(message.clone())),
                )
            }
        })
        .into_any_element()
}

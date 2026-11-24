use crate::{icons::*, state::AppState};
use nsh_base::{button, input};
use nsh_core::element::SubmitBehavior;
use nsh_core::prelude::*;
use serde_json::json;

fn action(app: AppState, label: &'static str, f: impl Fn() + 'static) -> impl IntoElement {
    let p = app.palette;
    button()
        .px(px(12.0))
        .h(px(30.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(move || p.line())
        .bg(move || p.panel())
        .hover(move |s| s.bg(move || p.hover()))
        .on_press(f)
        .items_center()
        .justify_center()
        .child(text(label).text_size(px(12.0)).text_color(move || p.ink()))
}
fn error(app: AppState) -> impl IntoElement {
    when(
        move || !app.error.get().is_empty(),
        move || {
            view()
                .px(px(24.0))
                .py(px(10.0))
                .gap(px(8.0))
                .child(
                    text(move || app.error.get())
                        .text_size(px(12.0))
                        .text_color(rgb(0xa45f65)),
                )
                .child(action(app, "重试", move || {
                    match app.page.get_untracked() {
                        crate::state::Page::Usage => {
                            app.load_usage_page(app.usage_offset.get_untracked())
                        }
                        crate::state::Page::Settings => app.load_preferences(),
                        _ => {
                            app.refresh_threads();
                            app.refresh_thread();
                            app.load_catalog();
                        }
                    }
                }))
        },
    )
}
pub fn conversation(app: AppState) -> impl IntoElement {
    let p = app.palette;
    view()
        .flex_1()
        .min_w(px(0.0))
        .child(
            view()
                .h(px(52.0))
                .px(px(20.0))
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .border_b_1()
                .border_b_color(move || p.line())
                .child(when(
                    move || !app.sidebar.get(),
                    move || {
                        button()
                            .accessibility_label("展开侧栏")
                            .p(px(4.0))
                            .on_press(move || app.sidebar.set(true))
                            .child(icon(PANEL))
                    },
                ))
                .child(
                    text(move || {
                        app.snapshot.with(|v| {
                            v["thread"]["title"]
                                .as_str()
                                .unwrap_or("新建对话")
                                .to_owned()
                        })
                    })
                    .flex_1()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || p.ink())
                    .number_of_lines(1),
                )
                .child(when(
                    move || !app.selected.get().is_empty(),
                    move || {
                        view()
                            .flex_row()
                            .gap(px(8.0))
                            .child(action(app, "重命名", move || {
                                app.title_draft.set(app.snapshot.with_untracked(|v| {
                                    v["thread"]["title"].as_str().unwrap_or("").to_owned()
                                }));
                                app.rename.set(true);
                            }))
                            .child(action(app, "归档 / 恢复", move || {
                                let archived = app
                                    .snapshot
                                    .with_untracked(|v| v["thread"]["archived"] == true);
                                app.thread_command(
                                    if archived {
                                        "conversation.thread.unarchive"
                                    } else {
                                        "conversation.thread.archive"
                                    },
                                    json!({}),
                                );
                            }))
                    },
                )),
        )
        .child(when(
            move || app.rename.get(),
            move || {
                view()
                    .px(px(24.0))
                    .py(px(12.0))
                    .flex_row()
                    .gap(px(8.0))
                    .child(
                        input()
                            .value(move || app.title_draft.get())
                            .on_change(move |v: &str| app.title_draft.set(v.to_owned()))
                            .flex_1()
                            .text_color(move || p.ink())
                            .border_1()
                            .border_color(move || p.line())
                            .rounded(px(7.0))
                            .px(px(10.0))
                            .h(px(34.0)),
                    )
                    .child(action(app, "保存", move || {
                        app.thread_command(
                            "conversation.thread.rename",
                            json!({"title":app.title_draft.get_untracked()}),
                        )
                    }))
                    .child(action(app, "取消", move || app.rename.set(false)))
            },
        ))
        .child(error(app))
        .child(
            when(
                move || app.selected.get().is_empty(),
                move || {
                    view()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap(px(12.0))
                        .child(
                            text("有什么可以帮你？")
                                .text_size(px(28.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(move || p.ink()),
                        )
                        .child(
                            text("开始一段新对话")
                                .text_size(px(14.0))
                                .text_color(move || p.muted()),
                        )
                },
            )
            .otherwise(move || {
                scroll().flex_1().child(
                    view()
                        .w_full()
                        .max_w(px(768.0))
                        .self_center()
                        .px(px(24.0))
                        .py(px(28.0))
                        .gap(px(24.0))
                        .child(when(
                            move || app.snapshot.with(|s| s.is_null()),
                            move || text("正在加载对话…").text_color(move || p.muted()),
                        ))
                        .child(each(
                            move || {
                                app.snapshot
                                    .with(|v| v["messages"].as_array().cloned().unwrap_or_default())
                            },
                            |m| m["message_id"].as_str().unwrap_or("").to_owned(),
                            move |message, _| {
                                let mine = message.with_untracked(|m| m["role"] == "user");
                                view()
                                    .gap(px(6.0))
                                    .map(|v| {
                                        if mine {
                                            v.self_end()
                                                .max_w(relative(0.85))
                                                .bg(move || p.sidebar())
                                                .rounded(px(12.0))
                                                .px(px(16.0))
                                                .py(px(12.0))
                                        } else {
                                            v.w_full()
                                        }
                                    })
                                    .child(
                                        text(if mine { "你" } else { "Muteki" })
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(move || p.muted()),
                                    )
                                    .child(
                                        when(
                                            move || mine,
                                            move || {
                                                text(move || {
                                                    message.with(|m| {
                                                        m["text"].as_str().unwrap_or("").to_owned()
                                                    })
                                                })
                                                .text_size(px(14.0))
                                                .line_height(px(23.0))
                                                .text_color(move || p.ink())
                                                .selectable(true)
                                            },
                                        )
                                        .otherwise(
                                            move || {
                                                crate::markdown::reply(p, move || {
                                                    message.with(|m| {
                                                        m["text"].as_str().unwrap_or("").to_owned()
                                                    })
                                                })
                                            },
                                        ),
                                    )
                            },
                        ))
                        .child(when(
                            move || app.running(),
                            move || {
                                text("正在思考…")
                                    .text_size(px(13.0))
                                    .text_color(move || p.muted())
                            },
                        )),
                )
            }),
        )
        .child(composer(app))
}
fn composer(app: AppState) -> impl IntoElement {
    let p = app.palette;
    view()
        .w_full()
        .max_w(px(768.0))
        .self_center()
        .px(px(24.0))
        .pt(px(12.0))
        .pb(px(12.0))
        .gap(px(8.0))
        .child(
            view()
                .bg(move || p.panel())
                .rounded(px(16.0))
                .border_1()
                .border_color(move || p.line())
                .p(px(12.0))
                .gap(px(8.0))
                .child(
                    input()
                        .test_id("composer")
                        .value(move || app.draft.get())
                        .on_change(move |s: &str| app.draft.set(s.to_owned()))
                        .placeholder("向 Muteki 发送消息…")
                        .multiline(true)
                        .submit_behavior(move || {
                            if app.prefs.with(|v| v["values"]["sendKey"] == "mod-enter") {
                                SubmitBehavior::Newline
                            } else {
                                SubmitBehavior::Submit
                            }
                        })
                        .min_h(px(60.0))
                        .max_h(px(180.0))
                        .w_full()
                        .text_size(px(14.0))
                        .text_color(move || p.ink())
                        .placeholder_color(move || p.muted())
                        .on_submit(move |_: &str| {
                            if app
                                .prefs
                                .with_untracked(|v| v["values"]["sendKey"] != "mod-enter")
                            {
                                app.send();
                            }
                        }),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(crate::model_picker::picker(app))
                        .child(
                            when(
                                move || app.running(),
                                move || {
                                    button()
                                        .accessibility_label("停止生成")
                                        .size(px(32.0))
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(10.0))
                                        .bg(move || p.hover())
                                        .on_press(move || {
                                            app.thread_command(
                                                "conversation.turn.interrupt",
                                                json!({}),
                                            )
                                        })
                                        .child(icon(STOP))
                                },
                            )
                            .otherwise(move || {
                                button()
                                    .accessibility_label("发送消息")
                                    .disabled(move || {
                                        app.draft.with(|s| s.trim().is_empty())
                                            || app.sending.get()
                                            || !crate::composer::supported(
                                                &app.catalog.get(),
                                                &app.credential.get(),
                                                &app.model.get(),
                                            )
                                            || app
                                                .snapshot
                                                .with(|v| v["thread"]["archived"] == true)
                                    })
                                    .size(px(32.0))
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(10.0))
                                    .bg(move || p.hover())
                                    .on_press(move || app.send())
                                    .child(icon(ARROW))
                            }),
                        ),
                ),
        )
        .child(
            text(move || {
                if app.sending.get() {
                    "正在发送…"
                } else {
                    if app.prefs.with(|v| v["values"]["sendKey"] == "mod-enter") {
                        "⌘ Enter 发送"
                    } else {
                        "Enter 发送 · Shift Enter 换行"
                    }
                }
            })
            .self_center()
            .text_size(px(11.0))
            .text_color(move || p.muted()),
        )
}
pub fn settings(app: AppState) -> impl IntoElement {
    let p = app.palette;
    scroll().flex_1().child(
        view()
            .w_full()
            .max_w(px(820.0))
            .self_center()
            .p(px(32.0))
            .gap(px(24.0))
            .child(
                text("设置")
                    .text_size(px(24.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || p.ink()),
            )
            .child(error(app))
            .child(
                text("外观")
                    .text_size(px(16.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || p.ink()),
            )
            .child(preference_row(
                app,
                "主题",
                "选择界面的显示方式",
                "theme",
                &[("system", "跟随系统"), ("light", "浅色"), ("dark", "深色")],
            ))
            .child(
                text("对话")
                    .text_size(px(16.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || p.ink()),
            )
            .child(preference_row(
                app,
                "发送快捷键",
                "选择发送消息的快捷键",
                "sendKey",
                &[("enter", "Enter"), ("mod-enter", "⌘ Enter")],
            ))
            .child(
                text("设置会保存到当前 Muteki 服务。")
                    .text_size(px(12.0))
                    .text_color(move || p.muted()),
            ),
    )
}
fn preference_row(
    app: AppState,
    title: &'static str,
    note: &'static str,
    key: &'static str,
    choices: &'static [(&'static str, &'static str)],
) -> impl IntoElement {
    let p = app.palette;
    view()
        .flex_row()
        .items_center()
        .gap(px(20.0))
        .py(px(16.0))
        .border_b_1()
        .border_b_color(move || p.line())
        .child(
            view()
                .flex_1()
                .gap(px(5.0))
                .child(text(title).text_size(px(14.0)).text_color(move || p.ink()))
                .child(text(note).text_size(px(12.0)).text_color(move || p.muted())),
        )
        .child(
            view()
                .flex_row()
                .gap(px(4.0))
                .child(choices.iter().map(move |&(value, label)| {
                    button()
                        .px(px(12.0))
                        .h(px(32.0))
                        .rounded(px(7.0))
                        .items_center()
                        .justify_center()
                        .bg(move || {
                            if app.prefs.with(|v| {
                                v["values"][key].as_str().unwrap_or(if key == "theme" {
                                    "system"
                                } else {
                                    "enter"
                                }) == value
                            }) {
                                p.hover()
                            } else {
                                p.bg()
                            }
                        })
                        .on_press(move || app.save_preference(key, value))
                        .child(text(label).text_size(px(12.0)).text_color(move || p.ink()))
                })),
        )
}
pub fn usage(app: AppState) -> impl IntoElement {
    crate::usage::dashboard(app)
}

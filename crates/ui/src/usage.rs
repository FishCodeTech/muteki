//! Ledger geometry and labels follow UsageDashboard.module.css / UsageLedger.
use crate::{state::AppState, theme::Palette};
use nsh_base::{button, table, table_body, table_cell, table_head, table_header, table_row};
use nsh_core::prelude::*;
use serde_json::Value;
pub fn number(value: &Value) -> String {
    match value.as_f64().filter(|v| v.is_finite()) {
        Some(v) => format!("{v:.0}"),
        None => "未提供".into(),
    }
}
fn cost(v: &Value) -> String {
    let (value, estimated) = if v["cost"].is_number() {
        (&v["cost"], false)
    } else if v["reported_cost"].is_number() {
        (&v["reported_cost"], false)
    } else {
        (&v["estimated_cost"], true)
    };
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .map(|v| format!("${v:.4}{}", if estimated { " (估算)" } else { "" }))
        .unwrap_or_else(|| "未提供".into())
}
fn date(v: &Value) -> String {
    v.as_f64()
        .filter(|v| v.is_finite())
        .and_then(|v| chrono::DateTime::<chrono::Utc>::from_timestamp(v as i64, 0))
        .map(|v| v.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "未提供".into())
}
fn cell(p: Palette, content: String, width: f32) -> impl IntoElement {
    table_cell().w(px(width)).px(px(10.0)).py(px(12.0)).child(
        text(content)
            .text_size(px(12.0))
            .text_color(move || p.ink())
            .number_of_lines(2),
    )
}
fn header(p: Palette, labels: &'static [(&'static str, f32)]) -> impl IntoElement {
    table_header().child(
        table_row()
            .border_b_1()
            .border_b_color(move || p.line())
            .child(labels.iter().map(move |&(label, width)| {
                table_head().w(px(width)).px(px(10.0)).py(px(10.0)).child(
                    text(label)
                        .text_size(px(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || p.muted()),
                )
            })),
    )
}
fn nav(app: AppState, title: &'static str, previous: bool) -> impl IntoElement {
    let p = app.palette;
    button()
        .disabled(move || {
            app.loading.get()
                || if previous {
                    app.usage_offset.get() == 0
                } else {
                    app.usage.with(|v| {
                        app.usage_offset.get() + 50 >= v["count"].as_u64().unwrap_or(0) as usize
                    })
                }
        })
        .h(px(32.0))
        .px(px(12.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(move || p.line())
        .items_center()
        .justify_center()
        .on_press(move || {
            app.load_usage_page(if previous {
                app.usage_offset.get_untracked().saturating_sub(50)
            } else {
                app.usage_offset.get_untracked() + 50
            })
        })
        .child(text(title).text_size(px(12.0)).text_color(move || p.ink()))
}
fn total_table(app: AppState) -> impl IntoElement {
    let p = app.palette;
    let rows = dynamic(move || {
        app.usage.with(|v| {
            let t = &v["totals"];
            table_row()
                .child(cell(p, number(&t["input_tokens"]), 120.0))
                .child(cell(p, number(&t["output_tokens"]), 120.0))
                .child(cell(p, number(&t["total_tokens"]), 120.0))
                .child(cell(p, number(&t["cache_read_tokens"]), 120.0))
                .child(cell(p, number(&t["cache_write_tokens"]), 120.0))
                .child(cell(p, number(&t["records"]), 100.0))
                .child(cell(p, cost(t), 140.0))
        })
    });
    scroll().horizontal().child(
        table()
            .accessibility_label("用量总计")
            .min_w(px(840.0))
            .child(header(
                p,
                &[
                    ("输入", 120.0),
                    ("输出", 120.0),
                    ("总 Token", 120.0),
                    ("缓存读取", 120.0),
                    ("缓存写入", 120.0),
                    ("记录数", 100.0),
                    ("金额", 140.0),
                ],
            ))
            .child(table_body().child(rows)),
    )
}
fn model_table(app: AppState) -> impl IntoElement {
    let p = app.palette;
    let rows = each(
        move || {
            app.usage
                .with(|v| v["groups"]["model"].as_array().cloned().unwrap_or_default())
        },
        |v| v["name"].as_str().unwrap_or("").to_owned(),
        move |row, _| {
            dynamic(move || {
                row.with(|v| {
                    table_row()
                        .border_b_1()
                        .border_b_color(move || p.line())
                        .child(cell(
                            p,
                            v["name"].as_str().unwrap_or("未上报").to_owned(),
                            280.0,
                        ))
                        .child(cell(p, number(&v["input_tokens"]), 120.0))
                        .child(cell(p, number(&v["output_tokens"]), 120.0))
                        .child(cell(p, number(&v["total_tokens"]), 120.0))
                        .child(cell(p, number(&v["records"]), 100.0))
                        .child(cell(p, cost(v), 140.0))
                })
            })
        },
    );
    scroll().horizontal().child(
        table()
            .accessibility_label("模型用量")
            .min_w(px(880.0))
            .child(header(
                p,
                &[
                    ("模型", 280.0),
                    ("输入", 120.0),
                    ("输出", 120.0),
                    ("总 Token", 120.0),
                    ("记录数", 100.0),
                    ("金额", 140.0),
                ],
            ))
            .child(table_body().child(rows)),
    )
}
fn record_table(app: AppState) -> impl IntoElement {
    let p = app.palette;
    let rows = each(
        move || {
            app.usage.with(|v| {
                v["records"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .enumerate()
                    .collect::<Vec<_>>()
            })
        },
        |(i, _)| *i,
        move |row, _| {
            dynamic(move || {
                row.with(|(_, v)| {
                    table_row()
                        .border_b_1()
                        .border_b_color(move || p.line())
                        .child(cell(
                            p,
                            format!(
                                "{}\n{}",
                                date(&v["at"]),
                                v["thread_id"]
                                    .as_str()
                                    .or_else(|| v["challenge_id"].as_str())
                                    .unwrap_or("未上报")
                            ),
                            230.0,
                        ))
                        .child(cell(
                            p,
                            format!(
                                "{}\n{}",
                                v["role"].as_str().unwrap_or("未上报"),
                                v["model"].as_str().unwrap_or("未上报")
                            ),
                            230.0,
                        ))
                        .child(cell(p, number(&v["input_tokens"]), 110.0))
                        .child(cell(p, number(&v["output_tokens"]), 110.0))
                        .child(cell(
                            p,
                            format!(
                                "{} / {}",
                                number(&v["cache_read_tokens"]),
                                number(&v["cache_write_tokens"])
                            ),
                            140.0,
                        ))
                        .child(cell(p, number(&v["reasoning_tokens"]), 100.0))
                        .child(cell(p, cost(v), 140.0))
                })
            })
        },
    );
    scroll().horizontal().child(
        table()
            .accessibility_label("用量明细")
            .min_w(px(1100.0))
            .child(header(
                p,
                &[
                    ("时间 / 归属", 230.0),
                    ("角色 / 模型", 230.0),
                    ("输入", 110.0),
                    ("输出", 110.0),
                    ("缓存读 / 写", 140.0),
                    ("推理", 100.0),
                    ("金额", 140.0),
                ],
            ))
            .child(table_body().child(rows)),
    )
}
pub fn dashboard(app: AppState) -> impl IntoElement {
    let p = app.palette;
    scroll().flex_1().child(
        view()
            .p(px(32.0))
            .gap(px(20.0))
            .w_full()
            .max_w(px(1400.0))
            .self_center()
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .child(
                        text("用量统计")
                            .flex_1()
                            .text_size(px(26.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(move || p.ink()),
                    )
                    .child(
                        button()
                            .accessibility_label("刷新用量")
                            .on_press(move || app.load_usage_page(app.usage_offset.get_untracked()))
                            .child(
                                text("刷新")
                                    .text_size(px(12.0))
                                    .text_color(move || p.muted()),
                            ),
                    ),
            )
            .child(
                text("当前服务记录的用量，不代表供应商账单。缺失数据不按零计算。")
                    .text_size(px(12.0))
                    .text_color(move || p.muted()),
            )
            .child(when(
                move || !app.error.get().is_empty(),
                move || {
                    text(move || app.error.get())
                        .text_size(px(12.0))
                        .text_color(rgb(0xa45f65))
                },
            ))
            .child(when(
                move || app.loading.get(),
                move || {
                    text("正在读取用量明细…")
                        .text_size(px(12.0))
                        .text_color(move || p.muted())
                },
            ))
            .child(
                text("总计")
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || p.ink()),
            )
            .child(total_table(app))
            .child(
                text("按模型分组")
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || p.ink()),
            )
            .child(model_table(app))
            .child(
                text("用量明细")
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || p.ink()),
            )
            .child(record_table(app))
            .child(when(
                move || app.usage.with(|v| !v.is_null() && v["count"] == 0),
                move || {
                    text("暂无用量记录")
                        .text_size(px(13.0))
                        .text_color(move || p.muted())
                },
            ))
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        text(move || {
                            app.usage
                                .with(|v| format!("共 {} 条记录", v["count"].as_u64().unwrap_or(0)))
                        })
                        .flex_1()
                        .text_size(px(11.0))
                        .text_color(move || p.muted()),
                    )
                    .child(nav(app, "上一页", true))
                    .child(nav(app, "下一页", false)),
            ),
    )
}

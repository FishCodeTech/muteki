use crate::{
    icons::*,
    state::{AppState, Page},
    surfaces,
};
use nsh_base::{button, input};
use nsh_core::element::FileAsset;
use nsh_core::prelude::*;

fn rail_button(
    app: AppState,
    asset: FileAsset,
    label: &'static str,
    page: Page,
) -> impl IntoElement {
    let p = app.palette;
    button()
        .accessibility_label(label)
        .size(px(38.0))
        .rounded(px(10.0))
        .items_center()
        .justify_center()
        .bg(move || {
            if app.page.get() == page {
                p.bg()
            } else {
                p.rail()
            }
        })
        .hover(move |s| s.bg(move || p.hover()))
        .on_press(move || {
            app.error.set(String::new());
            if page == Page::Usage {
                app.load_usage();
            } else {
                app.page.set(page);
            }
        })
        .child(icon(asset))
}
pub fn workspace(app: AppState) -> impl IntoElement {
    let p = app.palette;
    view()
        .flex_1()
        .flex_row()
        .bg(move || p.rail())
        .child(
            view()
                .w(px(56.0))
                .py(px(9.0))
                .items_center()
                .gap(px(8.0))
                .child(image(LOGO.uri).size(px(30.0)).mb(px(14.0)))
                .child(rail_button(app, CHAT, "对话", Page::Chat))
                .child(rail_button(app, ARCHIVE, "归档", Page::Archives))
                .child(view().flex_1())
                .child(rail_button(app, USAGE, "用量统计", Page::Usage))
                .child(rail_button(app, SETTINGS, "设置", Page::Settings)),
        )
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .my(px(8.0))
                .mr(px(8.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(move || p.line())
                .bg(move || p.bg())
                .overflow_hidden()
                .child(
                    when(
                        move || app.page.get() == Page::Chat || app.page.get() == Page::Archives,
                        move || {
                            view()
                                .flex_1()
                                .flex_row()
                                .child(when(move || app.sidebar.get(), move || sidebar(app)))
                                .child(surfaces::conversation(app))
                        },
                    )
                    .otherwise(move || {
                        when(
                            move || app.page.get() == Page::Settings,
                            move || crate::settings::settings(app),
                        )
                        .otherwise(move || surfaces::usage(app))
                    }),
                ),
        )
}
fn sidebar(app: AppState) -> impl IntoElement {
    let p = app.palette;
    view()
        .w(px(260.0))
        .min_w(px(220.0))
        .bg(move || p.sidebar())
        .border_r_1()
        .border_r_color(move || p.line())
        .child(
            view()
                .h(px(52.0))
                .flex_row()
                .items_center()
                .px(px(14.0))
                .gap(px(8.0))
                .child(
                    text("Muteki")
                        .text_size(px(14.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || p.ink())
                        .flex_1(),
                )
                .child(
                    button()
                        .accessibility_label("收起侧栏")
                        .p(px(6.0))
                        .rounded(px(6.0))
                        .hover(move |s| s.bg(move || p.hover()))
                        .on_press(move || app.sidebar.set(false))
                        .child(icon(PANEL)),
                ),
        )
        .child(
            button()
                .mx(px(8.0))
                .h(px(34.0))
                .px(px(10.0))
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .rounded(px(10.0))
                .hover(move |s| s.bg(move || p.hover()))
                .on_press(move || app.new_chat())
                .child(icon(PLUS))
                .child(
                    text("新建对话")
                        .text_size(px(14.0))
                        .text_color(move || p.ink()),
                ),
        )
        .child(
            view()
                .mx(px(10.0))
                .mt(px(10.0))
                .h(px(32.0))
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .child(icon(SEARCH))
                .child(
                    input()
                        .value(move || app.search.get())
                        .on_change(move |s: &str| app.search.set(s.to_owned()))
                        .placeholder("搜索对话")
                        .flex_1()
                        .text_size(px(12.0))
                        .text_color(move || p.ink())
                        .placeholder_color(move || p.muted()),
                ),
        )
        .child(
            view()
                .px(px(18.0))
                .pt(px(24.0))
                .pb(px(8.0))
                .flex_row()
                .child(
                    text(move || {
                        if app.page.get() == Page::Archives {
                            "归档对话"
                        } else {
                            "对话"
                        }
                    })
                    .text_size(px(11.0))
                    .text_color(move || p.muted())
                    .flex_1(),
                )
                .child(
                    button()
                        .accessibility_label("刷新对话")
                        .on_press(move || app.refresh_threads())
                        .child(icon(REFRESH)),
                ),
        )
        .child(
            scroll().flex_1().child(
                view()
                    .px(px(8.0))
                    .gap(px(2.0))
                    .child(when(
                        move || app.loading.get(),
                        move || {
                            text("正在加载…")
                                .p(px(10.0))
                                .text_size(px(12.0))
                                .text_color(move || p.muted())
                        },
                    ))
                    .child(each(
                        move || {
                            let query = app.search.get().to_lowercase();
                            let archived = app.page.get() == Page::Archives;
                            let mut rows: Vec<_> = app
                                .threads
                                .get()
                                .into_iter()
                                .filter(|t| {
                                    t["archived"].as_bool().unwrap_or(false) == archived
                                        && t["title"]
                                            .as_str()
                                            .unwrap_or("")
                                            .to_lowercase()
                                            .contains(&query)
                                })
                                .collect();
                            rows.sort_by(|a, b| {
                                b["updated_at"].as_str().cmp(&a["updated_at"].as_str())
                            });
                            rows
                        },
                        |t| t["thread_id"].as_str().unwrap_or("").to_owned(),
                        move |row, _| {
                            let id = row.with_untracked(|t| {
                                t["thread_id"].as_str().unwrap_or("").to_owned()
                            });
                            let selected_id = id.clone();
                            button()
                                .h(px(34.0))
                                .px(px(10.0))
                                .rounded(px(8.0))
                                .justify_center()
                                .bg(move || {
                                    if app.selected.get() == selected_id {
                                        p.hover()
                                    } else {
                                        p.sidebar()
                                    }
                                })
                                .hover(move |s| s.bg(move || p.hover()))
                                .on_press(move || app.open(id.clone()))
                                .child(
                                    text(move || {
                                        row.with(|t| {
                                            t["title"].as_str().unwrap_or("无标题对话").to_owned()
                                        })
                                    })
                                    .text_size(px(13.0))
                                    .text_color(move || p.ink())
                                    .number_of_lines(1),
                                )
                        },
                    ))
                    .child(when(
                        move || !app.loading.get() && app.threads.with(|t| t.is_empty()),
                        move || {
                            text("暂无对话")
                                .p(px(10.0))
                                .text_size(px(12.0))
                                .text_color(move || p.muted())
                        },
                    )),
            ),
        )
        .child(
            text("Muteki")
                .px(px(18.0))
                .py(px(16.0))
                .text_size(px(11.0))
                .text_color(move || p.muted()),
        )
}

use crate::state::AppState;
use nsh_base::{SelectItem, SelectItemState, SelectValue, select};
use nsh_core::{element::StyleRefinement, prelude::*};

pub fn picker(app: AppState) -> impl IntoElement {
    let p = app.palette;
    let make_style = move |s: StyleRefinement| {
        s.bg(move || p.panel())
            .border_1()
            .border_color(move || p.line())
            .rounded(px(10.0))
            .p(px(4.0))
    };
    view()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .flex_1()
        .min_w(px(0.0))
        .child(
            select()
                .accessibility_label("凭据")
                .searchable(true)
                .search_placeholder("搜索凭据")
                .disabled(move || app.running() || app.sending.get())
                .value(move || {
                    SelectValue::Controlled(
                        (!app.credential.get().is_empty()).then(|| app.credential.get().into()),
                    )
                })
                .items(move || {
                    app.catalog
                        .get()
                        .iter()
                        .map(|c| {
                            SelectItem::new(
                                c["credential_id"].as_str().unwrap_or("").to_owned(),
                                c["label"].as_str().unwrap_or("未命名凭据").to_owned(),
                            )
                            .disabled(c["http_compatible"] != true)
                        })
                        .collect::<Vec<_>>()
                })
                .popup_style(make_style)
                .render_item(move |s: &SelectItemState| {
                    text(s.title.clone())
                        .px(px(10.0))
                        .py(px(8.0))
                        .text_size(px(13.0))
                        .text_color(move || p.ink())
                        .bg(if s.selected || s.highlighted {
                            p.hover()
                        } else {
                            p.panel()
                        })
                        .rounded(px(6.0))
                })
                .trigger(
                    view()
                        .h(px(30.0))
                        .max_w(px(180.0))
                        .px(px(8.0))
                        .justify_center()
                        .rounded(px(7.0))
                        .hover(move |s| s.bg(move || p.hover()))
                        .child(
                            text(move || {
                                app.catalog.with(|rows| {
                                    rows.iter()
                                        .find(|c| c["credential_id"] == app.credential.get())
                                        .and_then(|c| c["label"].as_str())
                                        .unwrap_or(if app.credential.get().is_empty() {
                                            "选择凭据"
                                        } else {
                                            "当前凭据不可用"
                                        })
                                        .to_owned()
                                })
                            })
                            .number_of_lines(1)
                            .text_size(px(12.0))
                            .text_color(move || p.muted()),
                        ),
                )
                .empty(
                    text("没有可用凭据")
                        .p(px(12.0))
                        .text_color(move || p.muted()),
                )
                .on_change(move |value: &Option<SharedString>| {
                    if let Some(value) = value {
                        app.choose_credential(value.to_string());
                    }
                }),
        )
        .child(
            select()
                .accessibility_label("模型")
                .searchable(true)
                .search_placeholder("搜索模型")
                .disabled(move || {
                    app.running() || app.sending.get() || app.credential.get().is_empty()
                })
                .value(move || {
                    SelectValue::Controlled(
                        (!app.model.get().is_empty()).then(|| app.model.get().into()),
                    )
                })
                .items(move || {
                    app.catalog.with(|rows| {
                        rows.iter()
                            .find(|c| c["credential_id"] == app.credential.get())
                            .and_then(|c| c["models"].as_array())
                            .map(|models| {
                                models
                                    .iter()
                                    .map(|m| {
                                        SelectItem::new(
                                            m["id"].as_str().unwrap_or("").to_owned(),
                                            m["label"]
                                                .as_str()
                                                .or_else(|| m["id"].as_str())
                                                .unwrap_or("")
                                                .to_owned(),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    })
                })
                .popup_style(make_style)
                .render_item(move |s: &SelectItemState| {
                    text(s.title.clone())
                        .px(px(10.0))
                        .py(px(8.0))
                        .text_size(px(13.0))
                        .text_color(move || p.ink())
                        .bg(if s.selected || s.highlighted {
                            p.hover()
                        } else {
                            p.panel()
                        })
                        .rounded(px(6.0))
                })
                .trigger(
                    view()
                        .h(px(30.0))
                        .max_w(px(220.0))
                        .px(px(8.0))
                        .justify_center()
                        .rounded(px(7.0))
                        .hover(move |s| s.bg(move || p.hover()))
                        .child(
                            text(move || {
                                if app.model.get().is_empty() {
                                    "选择模型".into()
                                } else {
                                    app.model.get()
                                }
                            })
                            .number_of_lines(1)
                            .text_size(px(12.0))
                            .text_color(move || p.muted()),
                        ),
                )
                .empty(
                    text("此凭据没有可用模型")
                        .p(px(12.0))
                        .text_color(move || p.muted()),
                )
                .on_change(move |value: &Option<SharedString>| {
                    if let Some(value) = value {
                        app.choose_model(value.to_string());
                    }
                }),
        )
}

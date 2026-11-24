use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::icon::{Icon, IconName};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::theme::HighlightTheme;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::{ContextMenuExt as _, PopupMenu};
use agent::{ChangeCompleteness, FileChange, FileChangeKind};
use gpui::{
    AnyElement, App, Axis, ClickEvent, Div, ElementId, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement as _, ListHorizontalSizingBehavior, ListSizingBehavior,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledText, UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px,
    uniform_list,
};
use gpui_base::{ScrollableMask, h_flex, v_flex};

use super::super::model::{LiveEditRow, diff_stats};
use crate::diff::model::{DiffColors, FileDiffInput, RenderedRow, build_file, display_columns};
use crate::diff::parse::RowKind;

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub(crate) struct ChangedFilesHandlers {
    pub(crate) view_diff: ClickHandler,
    pub(crate) toggle_more: ClickHandler,
    pub(crate) open_files: Vec<ClickHandler>,
}

/// Finished-turn file summary with diff counts and file actions.
pub(crate) fn changed_files(
    index: usize,
    cwd: &Path,
    changes: &[FileChange],
    completeness: ChangeCompleteness,
    show_all: bool,
    handlers: ChangedFilesHandlers,
    cx: &App,
) -> AnyElement {
    let ChangedFilesHandlers {
        view_diff,
        toggle_more,
        open_files,
    } = handlers;

    let (total_add, total_del): (u32, u32) = changes
        .iter()
        .map(|change| diff_stats(change.diff.as_deref()))
        .fold(
            (0, 0),
            |(added, deleted), (change_added, change_deleted)| {
                (added + change_added, deleted + change_deleted)
            },
        );

    let header = h_flex()
        .w_full()
        .px_1()
        .gap_2()
        .items_center()
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_1p5()
                .items_center()
                .text_caption(cx)
                .child(crate::tr!("chat.changed_files", count = changes.len()))
                .when(completeness == ChangeCompleteness::Partial, |header| {
                    header.child(crate::tr!("chat.changed_files_partial"))
                })
                .child("·")
                .child(
                    div()
                        .text_color(cx.theme().success)
                        .child(format!("+{total_add}")),
                )
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(format!("-{total_del}")),
                ),
        )
        .child(
            quiet_control(
                ("view-diff", index),
                crate::tr!("chat.view_diff").into_owned().into(),
                view_diff,
                cx,
            )
            .tooltip(crate::tr!("chat.view_diff_tooltip").into_owned()),
        );

    let mut content = v_flex().w_full().gap_1p5().pt(px(2.)).child(header);

    let visible = if show_all {
        changes.len()
    } else {
        changes.len().min(3)
    };
    let mut body = h_flex().w_full().px_1().gap_1p5().flex_wrap();
    for (file_index, (change, on_click)) in changes.iter().zip(open_files).take(visible).enumerate()
    {
        let display = crate::workspace_walk::relativize_to_workspace(&change.path, cwd);
        let name = Path::new(&display)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| display.clone());
        let (added, deleted) = diff_stats(change.diff.as_deref());
        let chip = Button::new(SharedString::from(format!(
            "changed-file-chip-{index}-{file_index}"
        )))
        .ghost()
        .outline()
        .chip()
        .aria_label(format!("{}: {display}", crate::tr!("chat.view_diff")))
        .font_family(cx.theme().mono_font_family.clone())
        .text_token(&cx.theme().tokens.typography.xs)
        .child(name)
        .child(
            div()
                .text_color(cx.theme().success)
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(cx.theme().danger)
                .child(format!("-{deleted}")),
        );
        body = body.child(
            chip.on_click(on_click)
                .context_menu({
                    let path = change.path.clone();
                    let cwd = cwd.to_path_buf();
                    move |menu, _, _| {
                        menu.menu(
                            crate::tr!("chat.view_diff").into_owned(),
                            Box::new(super::super::OpenFileDiff {
                                turn: index,
                                path: path.clone(),
                            }),
                        )
                        .separator()
                        .path_items(&path, Some(&cwd))
                    }
                })
                .into_any_element(),
        );
    }
    if changes.len() > 3 {
        let hidden = changes.len() - 3;
        body = body.child(
            quiet_control(
                ("changed-files-more", index),
                if show_all {
                    crate::tr!("chat.show_fewer_files")
                } else {
                    crate::tr!("chat.more_files", count = hidden)
                }
                .into_owned()
                .into(),
                toggle_more,
                cx,
            )
            .font_family(cx.theme().mono_font_family.clone())
            .into_any_element(),
        );
    }
    content = content.child(body);

    content.into_any_element()
}

pub(crate) fn quiet_control(
    id: impl Into<ElementId>,
    label: SharedString,
    on_click: ClickHandler,
    cx: &App,
) -> Button {
    // The label is a child rather than `.label()`: a quiet control stays
    // muted, and `Button` paints its label in the variant's foreground.
    Button::new(id)
        .ghost()
        .small()
        .aria_label(label.clone())
        .text_token(&cx.theme().tokens.typography.xs)
        .on_click(on_click)
        .child(div().text_color(cx.theme().muted_foreground).child(label))
}

/// The `+N -N` pair, `flex_none` so it never gives ground to a long path.
fn diff_counts_colored(
    added: u32,
    deleted: u32,
    added_color: gpui::Hsla,
    deleted_color: gpui::Hsla,
    mono: SharedString,
    cx: &App,
) -> Div {
    h_flex()
        .flex_none()
        .gap_2()
        .text_token(&cx.theme().tokens.typography.xs)
        .font_family(mono)
        .child(div().text_color(added_color).child(format!("+{added}")))
        .child(div().text_color(deleted_color).child(format!("-{deleted}")))
}

#[derive(Clone)]
pub(crate) struct FileEditRowStyle {
    muted: gpui::Hsla,
    added: gpui::Hsla,
    deleted: gpui::Hsla,
    mono: SharedString,
}

impl FileEditRowStyle {
    pub(crate) fn from_theme(cx: &App) -> Self {
        Self {
            muted: cx.theme().muted_foreground,
            added: cx.theme().success,
            deleted: cx.theme().danger,
            mono: cx.theme().mono_font_family.clone(),
        }
    }
}

/// One live file-edit row: "Code edit  src/foo.rs  +12  -3". Provider diffs
/// drill down in place so the active edit can show its actual patch.
pub(crate) fn file_edit_row(
    key: &str,
    row: &LiveEditRow,
    cwd: &Path,
    expanded: bool,
    inline_diff: Option<AnyElement>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let style = FileEditRowStyle::from_theme(cx);
    let expandable = row.counts.is_some();
    let header = file_edit_row_header(row, expanded, expandable, &style, cx);
    let path_menu = {
        let path = row.path.clone();
        let cwd = cwd.to_path_buf();
        move |menu: PopupMenu, _: &mut Window, _: &mut gpui::Context<PopupMenu>| {
            menu.path_items(&path, Some(&cwd))
        }
    };
    let header: AnyElement = if expandable {
        crate::material::accessible_clickable(
            header,
            SharedString::from(format!("file-edit-row-{key}")),
            Role::Button,
            crate::tr!("chat.activity_details"),
            cx,
        )
        .aria_expanded(expanded)
        .rounded(crate::material::radius_chip(cx))
        .cursor_pointer()
        .hover(|header| header.bg(cx.theme().accent))
        .on_click(on_toggle)
        .context_menu(path_menu)
        .into_any_element()
    } else {
        header.context_menu(path_menu).into_any_element()
    };

    v_flex()
        .w_full()
        .gap_1()
        .child(header)
        .children(inline_diff)
        .into_any_element()
}

fn file_edit_row_header(
    row: &LiveEditRow,
    expanded: bool,
    expandable: bool,
    style: &FileEditRowStyle,
    cx: &App,
) -> Div {
    h_flex()
        .w_full()
        .min_h(px(crate::material::bar_height(
            crate::material::Bar::Section,
            false,
        )))
        .px_1()
        .gap_2()
        .items_center()
        .text_body(false, cx)
        .debug_selector(|| "file-edit-row".into())
        .child(Icon::new(IconName::File).text_color(style.muted))
        .child(
            h_flex()
                .min_w_0()
                .flex_1()
                .gap_1()
                .overflow_hidden()
                .child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .child(crate::tr!("chat.file_edit")),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(style.muted)
                        .font_family(style.mono.clone())
                        .debug_selector(|| "file-edit-path".into())
                        .child(crate::widgets::fade_text(row.path.clone())),
                ),
        )
        .when_some(row.counts, |element, (added, deleted)| {
            element.child(
                diff_counts_colored(
                    added,
                    deleted,
                    style.added,
                    style.deleted,
                    style.mono.clone(),
                    cx,
                )
                .debug_selector(|| "file-edit-counts".into()),
            )
        })
        .when(expandable, |element| {
            element.child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .text_color(style.muted),
            )
        })
}

/// The built inline diffs of expanded live file edits, by row key.
///
/// Building one diffs and syntax-highlights the whole patch, while its row
/// renders every frame and the patch changes only when the provider reports
/// more of it. An entry is rebuilt when its path, patch or theme changes, and
/// kept only while its row renders it: [`Self::sweep`] drops every entry the
/// frame before did not render, as GPUI drops the element state of an element
/// a frame leaves out, so a diff scrolled or folded away is built again.
pub(crate) struct InlineDiffCache {
    diffs: HashMap<String, InlineDiff>,
}

struct InlineDiff {
    path: String,
    kind: FileChangeKind,
    patch: Option<String>,
    theme: InlineDiffTheme,
    rows: Rc<[RenderedRow]>,
    widest: usize,
    scroll: UniformListScrollHandle,
    rendered: bool,
}

struct InlineDiffTheme {
    highlight: Arc<HighlightTheme>,
    added: Hsla,
    removed: Hsla,
}

impl InlineDiffTheme {
    fn from_theme(cx: &App) -> Self {
        Self {
            highlight: cx.theme().highlight_theme.clone(),
            added: cx.theme().success,
            removed: cx.theme().danger,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.highlight, &other.highlight)
            && self.added == other.added
            && self.removed == other.removed
    }
}

impl InlineDiffCache {
    pub(crate) fn new() -> Self {
        Self {
            diffs: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.diffs.clear();
    }

    /// Call once per frame, before the rows render.
    pub(crate) fn sweep(&mut self) {
        self.diffs
            .retain(|_, diff| std::mem::take(&mut diff.rendered));
    }

    pub(crate) fn render(&mut self, key: &str, row: &LiveEditRow, cx: &App) -> AnyElement {
        let theme = InlineDiffTheme::from_theme(cx);
        let current = self.diffs.get(key).is_some_and(|diff| {
            diff.path == row.path
                && diff.kind == row.kind
                && diff.patch == row.diff
                && diff.theme.matches(&theme)
        });
        if !current {
            let rendered = build_file(
                &FileDiffInput {
                    path: &row.path,
                    kind: row.kind,
                    old_text: None,
                    new_text: None,
                    patch: row.diff.as_deref(),
                    ignore_whitespace: false,
                    show_invisibles: false,
                },
                row.path.clone(),
                crate::highlight::language_name_for_path(&row.path),
                &theme.highlight,
                &DiffColors {
                    added_word_bg: theme.added.opacity(0.30),
                    removed_word_bg: theme.removed.opacity(0.28),
                },
                &HighlightStyle::default(),
            );
            let widest = rendered
                .all_rows
                .iter()
                .enumerate()
                .max_by_key(|(_, row)| display_columns(&row.text))
                .map_or(0, |(index, _)| index);
            let scroll = self
                .diffs
                .remove(key)
                .map_or_else(UniformListScrollHandle::new, |diff| diff.scroll);
            self.diffs.insert(
                key.to_string(),
                InlineDiff {
                    path: row.path.clone(),
                    kind: row.kind,
                    patch: row.diff.clone(),
                    theme,
                    rows: rendered.all_rows.into(),
                    widest,
                    scroll,
                    rendered: false,
                },
            );
        }
        let diff = self.diffs.get_mut(key).expect("built above");
        diff.rendered = true;
        render_inline_diff(key, diff, cx)
    }
}

fn render_inline_diff(key: &str, diff: &InlineDiff, cx: &App) -> AnyElement {
    let id = ElementId::from(SharedString::from(format!("file-edit-diff-{key}")));
    let scroll = diff.scroll.0.borrow().base_handle.clone();
    let rows = diff.rows.clone();
    let lines = uniform_list(id.clone(), rows.len(), move |range, _, cx| {
        range
            .map(|index| render_inline_diff_row(index, &rows[index], cx))
            .collect::<Vec<_>>()
    })
    .track_scroll(&diff.scroll)
    .with_sizing_behavior(ListSizingBehavior::Infer)
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(Some(diff.widest))
    // The masks move the offset and the list only clips: GPUI's own wheel
    // handling would move the timeline with it and map vertical input onto
    // the horizontal axis of a diff too short to scroll vertically.
    .overflow_hidden()
    .w_full()
    .max_h(px(240.));

    crate::material::rail_detail(
        div()
            .relative()
            .w_full()
            .min_w_0()
            .child(lines)
            .child(ScrollableMask::new(Axis::Vertical, &scroll).id(id.clone()))
            .child(ScrollableMask::new(Axis::Horizontal, &scroll).id(id)),
        cx,
    )
    .debug_selector(|| "file-edit-diff".into())
    .into_any_element()
}

fn render_inline_diff_row(index: usize, row: &RenderedRow, cx: &App) -> AnyElement {
    let (background, accent) = match row.kind {
        RowKind::Added => (
            Some(cx.theme().success.opacity(0.13)),
            Some(cx.theme().success),
        ),
        RowKind::Removed => (
            Some(cx.theme().danger.opacity(0.12)),
            Some(cx.theme().danger),
        ),
        RowKind::Context => (None, None),
    };
    let gutter = |line: Option<u32>| {
        div()
            .flex_none()
            .w(px(36.))
            .px_1()
            .text_right()
            .text_secondary(false, cx)
            .child(line.map(|line| line.to_string()).unwrap_or_default())
    };

    h_flex()
        .debug_selector(move || format!("file-edit-diff-line-{index}"))
        .min_w_full()
        .font_family(cx.theme().mono_font_family.clone())
        .text_token(&cx.theme().tokens.typography.xs)
        .items_start()
        .border_l_2()
        .border_color(accent.unwrap_or(gpui::transparent_black()))
        .when_some(background, |line, background| line.bg(background))
        .child(gutter(row.old))
        .child(gutter(row.new))
        .child(
            div()
                .flex_1()
                .px_2()
                .whitespace_nowrap()
                .text_color(cx.theme().foreground)
                .child(StyledText::new(row.text.clone()).with_highlights(row.runs.iter().cloned())),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::model::LiveEditRow;
    use gpui::TestAppContext;

    struct FileEditRowProbe {
        row: LiveEditRow,
    }

    impl gpui::Render for FileEditRowProbe {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            // The Work Log stacks rows in a full-width column, so the row is
            // content-height there; reproduce that rather than letting the
            // window stretch the row and mask a wrap.
            use gpui::{ParentElement as _, Styled as _};
            let key = "test-file-edit";
            gpui_base::v_flex().size_full().child(file_edit_row(
                key,
                &self.row,
                std::path::Path::new("/"),
                false,
                None,
                |_, _, _| {},
                cx,
            ))
        }
    }

    #[gpui::test]
    fn long_edit_path_and_counts_stay_inside_the_row_at_narrow_widths(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| FileEditRowProbe {
            row: LiveEditRow {
                path: "crates/ui/src/deeply/nested/module/tree/with/an/absurdly/long/name/live_file_edit_row.rs".into(),
                kind: agent::FileChangeKind::Modify,
                counts: Some((128, 96)),
                diff: None,
            },
        });
        let cx: &mut VisualTestContext = cx;
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
        };

        cx.simulate_resize(size(px(900.), px(80.)));
        draw(cx);
        let baseline = cx.debug_bounds("file-edit-row").expect("row bounds");
        let baseline_counts = cx.debug_bounds("file-edit-counts").expect("count bounds");

        // The chat column gets narrow when the sidebar and a right panel are
        // both open; sweep down to well below any practical chat width.
        for width in 280..=900 {
            cx.simulate_resize(size(px(width as f32), px(80.)));
            draw(cx);

            let row = cx.debug_bounds("file-edit-row").expect("row bounds");
            let path = cx.debug_bounds("file-edit-path").expect("path bounds");
            let counts = cx.debug_bounds("file-edit-counts").expect("count bounds");

            assert!(
                path.left() >= row.left() && path.right() <= row.right(),
                "path escaped the row at {width}px: row={row:?}, path={path:?}"
            );
            assert!(
                counts.left() >= row.left() && counts.right() <= row.right(),
                "+/- counts escaped the row at {width}px: row={row:?}, counts={counts:?}"
            );
            assert!(
                counts.left() >= path.right(),
                "+/- counts overlapped the path at {width}px: path={path:?}, counts={counts:?}"
            );
            assert_eq!(
                counts.size.width, baseline_counts.size.width,
                "+/- counts were squeezed at {width}px: {counts:?}"
            );
            assert_eq!(
                row.size.height, baseline.size.height,
                "row grew taller at {width}px, so the long path wrapped: row={row:?}"
            );
        }
    }
}

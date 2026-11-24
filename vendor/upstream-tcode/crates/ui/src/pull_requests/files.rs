//! The Files view: a pull request's changed files drawn by the diff list, from the host's
//! files read and never from the local checkout.

use std::ops::Range;
use std::rc::Rc;

use agent::FileChangeKind;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, HighlightStyle, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;
use tcode_core::session::ReviewSide;
use tcode_protocol::{
    PullRequestFile, PullRequestFileText, PullRequestPatch, PullRequestRead,
    PullRequestReadResponse, PullRequestReviewThread, PullRequestViewedState,
};

use super::detail::{PullRequestView, TextState, reason};
use crate::{
    diff::{
        list::{
            DiffList, DiffListHost, DiffSelectionMenu, FileLayout, FileMenu, GapState, kind_rail,
            render_list, render_notice,
        },
        model::{
            DiffColors, ExpandDir, FileDiffInput, RenderedFile, build_file, reconstruct_from_text,
        },
    },
    icon::{Icon, IconName},
    material,
    overlay::{Notification, OverlayExt as _},
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Popover,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        input::{Input, InputState},
        menu::{CopyText, DropdownMenu as _, OpenUrl},
        tooltip::Tooltip,
    },
};

/// A Files view toggle; the phone's overflow menu addresses them by action.
#[derive(Action, Clone, Copy, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_pull_requests, no_json)]
enum FilesOption {
    Split,
    Wrap,
    Whitespace,
    Invisibles,
    CollapseAll,
    ExpandAll,
    /// The phone's way to the Review sheet.
    Review,
}

/// How a file's viewed mark reads to the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Viewed {
    Marked,
    Unmarked,
    Changed,
    Unknown,
}

/// A column of files beside the diff once it has this much room.
const FILE_COLUMN_MIN_WIDTH: f32 = 720.;

impl PullRequestView {
    fn head(&self) -> Option<String> {
        self.page()
            .and_then(|page| page.files.data.as_ref())
            .map(|files| files.head.clone())
    }

    /// The file's text at the revision Files is read for, on GitHub.
    pub(super) fn file_url(&self, path: &str, cx: &App) -> Option<String> {
        let url = self.url(cx)?;
        let head = self.head()?;
        let repository = url.rsplit_once("/pull/")?.0.to_owned();
        Some(format!("{repository}/blob/{head}/{path}"))
    }

    fn viewed_state(&self, path: &str) -> Viewed {
        let Some(page) = self.page() else {
            return Viewed::Unknown;
        };
        if let Some(mark) = page.viewed_marks.get(path) {
            return if *mark {
                Viewed::Marked
            } else {
                Viewed::Unmarked
            };
        }
        let Some(viewed) = page.viewed.data.as_ref() else {
            return Viewed::Unknown;
        };
        match viewed.files.iter().find(|(file, _)| file == path) {
            Some((_, PullRequestViewedState::Viewed)) => Viewed::Marked,
            Some((_, PullRequestViewedState::Dismissed)) => Viewed::Changed,
            Some((_, PullRequestViewedState::Unviewed)) => Viewed::Unmarked,
            None if viewed.complete => Viewed::Unmarked,
            None => Viewed::Unknown,
        }
    }

    fn collapsed(&self, path: &str) -> bool {
        self.page()
            .and_then(|page| page.files_view.collapsed.get(path).copied())
            .unwrap_or_else(|| self.viewed_state(path) == Viewed::Marked)
    }

    fn layouts(&self) -> Vec<FileLayout> {
        self.files_of()
            .unwrap_or_default()
            .iter()
            .map(|file| {
                if !matches!(file.patch, PullRequestPatch::Hunks(_)) {
                    FileLayout::Notice
                } else if self.collapsed(&file.path) {
                    FileLayout::Header
                } else {
                    FileLayout::Rows
                }
            })
            .collect()
    }

    /// Renders the files the host answered with, once per head, page and option change.
    fn ensure_list(&mut self, cx: &mut Context<Self>) {
        let dark = cx.theme().mode.is_dark();
        let (ignore_ws, show_invisibles) = (self.ignore_ws, self.show_invisibles);
        let layouts = self.layouts();
        let Some(page) = self.page_mut() else { return };
        let Some(files) = page.files.data.as_ref() else {
            return;
        };
        let key = (
            files.head.clone(),
            files.files.len(),
            dark,
            ignore_ws,
            show_invisibles,
        );
        if page.files_view.rendered.as_ref() == Some(&key) {
            if let Some(list) = page.files_view.list.as_mut() {
                list.set_layouts(layouts);
            }
            return;
        }
        if page.files_view.rendering {
            return;
        }
        // A new page only appends: the files already drawn keep their rows and scroll.
        let kept = page
            .files_view
            .rendered
            .as_ref()
            .filter(|rendered| {
                (&rendered.0, rendered.2, rendered.3, rendered.4) == (&key.0, key.2, key.3, key.4)
            })
            .map_or(0, |rendered| rendered.1);
        let pending: Vec<_> = files.files[kept..]
            .iter()
            .map(|file| {
                let text = match page.files_view.texts.get(&file.path) {
                    Some(TextState::Loaded { old, new }) => Some((old.clone(), new.clone())),
                    _ => None,
                };
                (file.clone(), text)
            })
            .collect();
        page.files_view.rendering = true;
        let theme = cx.theme().highlight_theme.clone();
        let colors = DiffColors {
            added_word_bg: cx.theme().success.opacity(0.30),
            removed_word_bg: cx.theme().danger.opacity(0.28),
        };
        let whitespace = HighlightStyle {
            color: Some(cx.theme().muted_foreground),
            ..Default::default()
        };
        let current = self.current.clone();
        cx.spawn(async move |this, cx| {
            let rendered = cx
                .background_executor()
                .spawn(async move {
                    pending
                        .iter()
                        .map(|(file, text)| {
                            render(
                                file,
                                text.as_ref(),
                                ignore_ws,
                                show_invisibles,
                                &theme,
                                &colors,
                                &whitespace,
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let layouts = this.layouts();
                let Some(page) = current.and_then(|current| this.pages.get_mut(&current)) else {
                    return;
                };
                page.files_view.rendering = false;
                if page.files.data.as_ref().map(|files| files.head.as_str()) != Some(key.0.as_str())
                {
                    return;
                }
                match page.files_view.list.as_mut().filter(|_| kept > 0) {
                    Some(list) => {
                        list.append(rendered, layouts);
                    }
                    None => page.files_view.list = Some(DiffList::new(rendered, layouts, cx)),
                }
                page.files_view.rendered = Some(key);
                cx.notify();
            });
        })
        .detach();
    }

    fn file_index(&self, path: &str) -> Option<usize> {
        self.files_of()?.iter().position(|file| file.path == path)
    }

    /// Reads the file's text at the head and reverse-applies the patch to it for the base
    /// side, which is the merge base the hunks were cut against, not the base branch's tip.
    fn read_text(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(head) = self.head() else { return };
        let Some(entry) = self.files_of().and_then(|files| files.get(file)).cloned() else {
            return;
        };
        let path = entry.path.clone();
        let patch = match &entry.patch {
            PullRequestPatch::Hunks(hunks) => hunks.clone(),
            _ => String::new(),
        };
        let Some(page) = self.page_mut() else { return };
        // A deleted file has no head side; its whole base side is the patch.
        if entry.kind == FileChangeKind::Delete {
            let state = match reconstruct_from_text(String::new(), &patch) {
                Some((old, new)) => TextState::Loaded { old, new },
                None => TextState::Unreadable,
            };
            page.files_view.texts.insert(path, state);
            cx.notify();
            return;
        }
        page.files_view
            .texts
            .insert(entry.path.clone(), TextState::Loading);
        let read = PullRequestRead::FileText {
            revision: head.clone(),
            path: path.clone(),
        };
        let created = entry.kind == FileChangeKind::Create;
        self.read(read, cx, move |page, result| {
            let state = match result {
                Ok((PullRequestReadResponse::FileText(PullRequestFileText::Text(text)), _)) => {
                    if created {
                        TextState::Loaded {
                            old: String::new(),
                            new: text,
                        }
                    } else {
                        match reconstruct_from_text(text, &patch) {
                            Some((old, new)) => TextState::Loaded { old, new },
                            None => TextState::Unreadable,
                        }
                    }
                }
                Ok((PullRequestReadResponse::FileText(PullRequestFileText::Missing), _)) => {
                    TextState::Missing
                }
                Ok((PullRequestReadResponse::FileText(PullRequestFileText::Oversized), _)) => {
                    TextState::Oversized
                }
                Ok(_) => TextState::Unreadable,
                Err(error) => {
                    page.files_view.texts.remove(&path);
                    page.files_view.pending_expand = None;
                    page.files_view.text_failure = Some(
                        crate::tr!(
                            "pull_requests.files.text_failed",
                            path = path,
                            revision = head.get(..7).unwrap_or(&head).to_owned(),
                            reason = reason(&error)
                        )
                        .into_owned(),
                    );
                    return;
                }
            };
            page.files_view.texts.insert(path, state);
        });
    }

    /// Redraws a file once its text arrived, then applies the expansion that asked for it.
    fn apply_text(&mut self, cx: &mut Context<Self>) {
        let dark_theme = cx.theme().highlight_theme.clone();
        let colors = DiffColors {
            added_word_bg: cx.theme().success.opacity(0.30),
            removed_word_bg: cx.theme().danger.opacity(0.28),
        };
        let whitespace = HighlightStyle {
            color: Some(cx.theme().muted_foreground),
            ..Default::default()
        };
        let (ignore_ws, show_invisibles) = (self.ignore_ws, self.show_invisibles);
        let Some(page) = self.page_mut() else { return };
        let Some((path, start, direction)) = page.files_view.pending_expand.clone() else {
            return;
        };
        let Some(TextState::Loaded { old, new }) = page.files_view.texts.get(&path) else {
            if !matches!(page.files_view.texts.get(&path), Some(TextState::Loading)) {
                page.files_view.pending_expand = None;
            }
            return;
        };
        let Some(files) = page.files.data.as_ref() else {
            return;
        };
        let Some(index) = files.files.iter().position(|file| file.path == path) else {
            return;
        };
        let rendered = render(
            &files.files[index],
            Some(&(old.clone(), new.clone())),
            ignore_ws,
            show_invisibles,
            &dark_theme,
            &colors,
            &whitespace,
        );
        page.files_view.pending_expand = None;
        let Some(list) = page.files_view.list.as_mut() else {
            return;
        };
        if let Some(slot) = list.files.get_mut(index) {
            *slot = rendered;
        }
        let gap = list.fold_for_line(index, start);
        match gap {
            Some(gap) => list.expand(index, gap, direction),
            None => list.relayout(),
        }
        cx.notify();
    }

    fn mark_viewed(
        &mut self,
        path: String,
        viewed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.page_mut() else { return };
        page.viewed_marks.insert(path.clone(), viewed);
        page.viewed_batch.insert(path.clone(), viewed);
        // Marking collapses a file and unmarking opens it; the reader's own choice yields.
        page.files_view.collapsed.remove(&path);
        if page.viewed_flush.is_none() {
            page.viewed_flush = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(400))
                    .await;
                let _ = this.update_in(cx, |this, window, cx| this.flush_viewed(window, cx));
            }));
        }
        cx.notify();
    }

    /// Sends the marks pressed within one beat, one command per direction.
    fn flush_viewed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session, key)) = self.current.clone() else {
            return;
        };
        let Some(page) = self.page_mut() else { return };
        page.viewed_flush = None;
        let batch = std::mem::take(&mut page.viewed_batch);
        for viewed in [true, false] {
            let paths: Vec<_> = batch
                .iter()
                .filter(|(_, mark)| **mark == viewed)
                .map(|(path, _)| path.clone())
                .collect();
            if paths.is_empty() {
                continue;
            }
            let task = self.store.update(cx, |store, cx| {
                store.command(
                    tcode_protocol::Command::SetPullRequestFilesViewed {
                        session_id: session.clone(),
                        key: key.clone(),
                        paths: paths.clone(),
                        viewed,
                    },
                    cx,
                )
            });
            let current = (session.clone(), key.clone());
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(page) = this.pages.get_mut(&current) else {
                        return;
                    };
                    if let Err(error) = result {
                        for path in &paths {
                            page.viewed_marks.remove(path);
                        }
                        window.push_notification(
                            Notification::error(
                                crate::tr!(
                                    "pull_requests.files.viewed_failed",
                                    reason = reason(&error)
                                )
                                .into_owned(),
                            ),
                            cx,
                        );
                    }
                    // The host dropped its viewed read; read the marks it now holds.
                    page.viewed.expires_at = 0;
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn set_all_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let paths: Vec<_> = self
            .files_of()
            .unwrap_or_default()
            .iter()
            .map(|file| file.path.clone())
            .collect();
        if let Some(page) = self.page_mut() {
            for path in paths {
                page.files_view.collapsed.insert(path, collapsed);
            }
        }
        cx.notify();
    }

    fn apply_option(&mut self, option: FilesOption, window: &mut Window, cx: &mut Context<Self>) {
        match option {
            FilesOption::Split => {
                let split = self.store.read(cx).diff_split();
                self.store
                    .update(cx, |store, cx| store.set_diff_split(!split, cx));
            }
            FilesOption::Wrap => self
                .store
                .update(cx, |store, cx| store.toggle_diff_wrap(cx)),
            FilesOption::Whitespace => self.ignore_ws = !self.ignore_ws,
            FilesOption::Invisibles => self.show_invisibles = !self.show_invisibles,
            FilesOption::CollapseAll => self.set_all_collapsed(true, cx),
            FilesOption::ExpandAll => self.set_all_collapsed(false, cx),
            FilesOption::Review => self.open_review(window, cx),
        }
        if let Some(list) = self.page().and_then(|page| page.files_view.list.as_ref()) {
            list.remeasure();
        }
        cx.notify();
    }

    fn on_option(&mut self, option: &FilesOption, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_option(*option, window, cx);
    }

    pub(super) fn on_selection_menu(
        &mut self,
        action: &DiffSelectionMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *action == DiffSelectionMenu::AddComment {
            self.start_line_comment(window, cx);
            return;
        }
        let text = self
            .page()
            .and_then(|page| page.files_view.list.as_ref())
            .map(DiffList::selected_lines)
            .unwrap_or_default();
        if !text.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    fn scroll_to(&mut self, file: usize, cx: &mut Context<Self>) {
        if let Some(list) = self.page().and_then(|page| page.files_view.list.as_ref()) {
            list.scroll_to_file(file);
        }
        cx.notify();
    }

    /// Threads drawn on the diff: current anchors on a file whose lines are drawn.
    fn inline_threads(&self) -> Vec<&PullRequestReviewThread> {
        let (Some(page), Some(head)) = (self.page(), self.head()) else {
            return Vec::new();
        };
        let Some(conversation) = page.conversation.data.as_ref() else {
            return Vec::new();
        };
        let list = page.files_view.list.as_ref();
        conversation
            .threads
            .iter()
            .filter(|thread| {
                let Some(anchor) = thread.anchor.as_ref().filter(|_| !thread.outdated) else {
                    return false;
                };
                if anchor.revision != head {
                    return false;
                }
                let Some(index) = self.file_index(&anchor.path) else {
                    return false;
                };
                list.and_then(|list| list.files.get(index))
                    .filter(|_| list.is_some_and(|list| list.layout(index) == FileLayout::Rows))
                    .is_some_and(|file| {
                        file.all_rows.iter().any(|row| match anchor.side {
                            ReviewSide::Old => row.old == Some(anchor.end_line),
                            ReviewSide::New => row.new == Some(anchor.end_line),
                        })
                    })
            })
            .collect()
    }

    fn off_diff(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let page = self.page()?;
        let conversation = page.conversation.data.as_ref()?;
        let inline: Vec<_> = self
            .inline_threads()
            .into_iter()
            .map(|thread| thread.id.clone())
            .collect();
        let threads: Vec<_> = conversation
            .threads
            .iter()
            .filter(|thread| !inline.contains(&thread.id))
            .cloned()
            .collect();
        let pending = self.off_diff_pending(cx);
        if threads.is_empty() && pending.is_empty() {
            return None;
        }
        let page = self.page()?;
        let more = page
            .files
            .data
            .as_ref()
            .is_some_and(|files| files.next_cursor.is_some());
        let count = threads.len() + pending.len();
        let label = if more {
            crate::tr!(
                "pull_requests.files.off_diff_loaded",
                count = count.to_string()
            )
        } else if count == 1 {
            crate::tr!("pull_requests.files.off_diff_one")
        } else {
            crate::tr!("pull_requests.files.off_diff", count = count.to_string())
        }
        .into_owned();
        let open = page.files_view.off_diff_open;
        let muted = cx.theme().muted_foreground;
        let xs = cx.theme().tokens.typography.xs;
        Some(
            // `card` insets its content, and these rows are full-bleed hover targets that
            // carry the card inset themselves (`card_header_row`).
            v_flex()
                .flex_none()
                .mx_2()
                .mb_1()
                .rounded(material::radius_card(cx))
                .bg(cx.theme().muted)
                .child(
                    material::accessible_clickable(
                        material::card_header_row(cx),
                        "pr-off-diff",
                        gpui::Role::Button,
                        label.clone(),
                        cx,
                    )
                    .aria_expanded(open)
                    .text_token(&xs)
                    .cursor_pointer()
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .text_color(muted),
                    )
                    .child(label)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(page) = this.page_mut() {
                            page.files_view.off_diff_open = !page.files_view.off_diff_open;
                        }
                        cx.notify();
                    })),
                )
                .child(
                    gpui_base::Collapsible::new().open(open).content(
                        v_flex()
                            .id("pr-off-diff-rows")
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .pb_1()
                            .children(pending)
                            .children(threads.into_iter().map(|thread| {
                                let line = thread
                                    .anchor
                                    .as_ref()
                                    .map(|anchor| format!("{}:{}", thread.path, anchor.end_line))
                                    .unwrap_or_else(|| thread.path.clone());
                                let excerpt = thread
                                    .comments
                                    .first()
                                    .map(|comment| {
                                        comment.body.lines().next().unwrap_or_default().to_owned()
                                    })
                                    .unwrap_or_default();
                                let id = thread.id.clone();
                                material::accessible_clickable(
                                    material::card_header_row(cx),
                                    SharedString::from(format!("pr-off-diff-{}", thread.id)),
                                    gpui::Role::Button,
                                    format!("{line} {excerpt}"),
                                    cx,
                                )
                                .text_token(&xs)
                                .cursor_pointer()
                                .hover(|row| row.bg(cx.theme().list_hover))
                                .child(
                                    div()
                                        .flex_none()
                                        .max_w(px(220.))
                                        .overflow_hidden()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .child(crate::widgets::fade_text(line)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_color(muted)
                                        .child(crate::widgets::fade_text(excerpt)),
                                )
                                .when(thread.outdated, |row| {
                                    row.child(material::semantic_chip(
                                        crate::tr!("pull_requests.conversation.outdated")
                                            .into_owned(),
                                        cx.theme().secondary,
                                        muted,
                                        cx,
                                    ))
                                })
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.show_thread(&id, cx);
                                    },
                                ))
                            })),
                    ),
                )
                .into_any_element(),
        )
    }

    fn viewed_box(
        &self,
        file: usize,
        labelled: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let entry = self.files_of()?.get(file)?.clone();
        let compact = self.compact(cx);
        let state = self.viewed_state(&entry.path);
        let read_only = self
            .store
            .read(cx)
            .session_status()
            .is_none_or(|status| status.conversation_read_only);
        let path = entry.path.clone();
        if state == Viewed::Unknown {
            let unreadable = self.page().is_some_and(|page| page.viewed.error.is_some());
            return Some(
                div()
                    .id(SharedString::from(format!("pr-viewed-unknown-{file}")))
                    .flex_none()
                    .child(
                        Icon::new(IconName::CircleQuestionMark)
                            .text_color(cx.theme().muted_foreground),
                    )
                    .tooltip(move |window, cx| {
                        Tooltip::new(
                            crate::tr!(if unreadable {
                                "pull_requests.files.viewed_unreadable"
                            } else {
                                "pull_requests.files.viewed_unknown"
                            })
                            .into_owned(),
                        )
                        .build(window, cx)
                    })
                    .into_any_element(),
            );
        }
        let checked = state == Viewed::Marked;
        let tooltip = if read_only {
            crate::tr!("pull_requests.files.viewed_read_only")
        } else if state == Viewed::Changed {
            crate::tr!("pull_requests.files.changed_since_viewed_tooltip")
        } else if checked {
            crate::tr!("pull_requests.files.mark_unviewed")
        } else {
            crate::tr!("pull_requests.files.mark_viewed")
        }
        .into_owned();
        let view = cx.entity();
        let mut checkbox = Checkbox::new(SharedString::from(format!("pr-viewed-{file}")))
            .checked(checked)
            .disabled(read_only)
            .on_click(move |checked, window, cx| {
                let path = path.clone();
                let checked = *checked;
                view.update(cx, |view, cx| view.mark_viewed(path, checked, window, cx));
            });
        if labelled && !compact {
            checkbox = checkbox.label(crate::tr!("pull_requests.files.viewed"));
        }
        Some(
            div()
                .id(SharedString::from(format!("pr-viewed-box-{file}")))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .text_token(&cx.theme().tokens.typography.xs)
                .when(compact, |cell| cell.size(px(material::TOUCH_TARGET)))
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(checkbox)
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .into_any_element(),
        )
    }

    /// "%{count} files", with a "+" while pages remain.
    fn count_label(&self) -> String {
        let files = self.page().and_then(|page| page.files.data.as_ref());
        let count = files.map_or(0, |files| files.files.len());
        if files.is_some_and(|files| files.next_cursor.is_some()) {
            crate::tr!("pull_requests.files.count_more", count = count.to_string())
        } else if count == 1 {
            crate::tr!("pull_requests.files.count_one")
        } else {
            crate::tr!("pull_requests.files.count", count = count.to_string())
        }
        .into_owned()
    }

    fn toolbar(&self, wide_column: bool, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let page = self.page();
        let files = page.and_then(|page| page.files.data.as_ref());
        let count_label = self.count_label();
        let viewed_counter = files.map(|files| {
            // Marks GitHub reports for files not paged in yet count too, out of every file.
            let mut paths: std::collections::HashSet<&str> = page
                .and_then(|page| page.viewed.data.as_ref())
                .into_iter()
                .flat_map(|viewed| viewed.files.iter().map(|(path, _)| path.as_str()))
                .collect();
            paths.extend(
                page.into_iter()
                    .flat_map(|page| page.viewed_marks.keys().map(String::as_str)),
            );
            let viewed = paths
                .into_iter()
                .filter(|path| self.viewed_state(path) == Viewed::Marked)
                .count();
            let total = files.changed_files.max(files.files.len() as u64);
            let unknown = files
                .files
                .iter()
                .any(|file| self.viewed_state(&file.path) == Viewed::Unknown)
                || page
                    .and_then(|page| page.viewed.data.as_ref())
                    .is_some_and(|viewed| !viewed.complete);
            let host_marks = self.capabilities().host_viewed_marks;
            let tooltip = if unknown {
                crate::tr!("pull_requests.files.viewed_unknown")
            } else if host_marks {
                crate::tr!("pull_requests.files.viewed_owner_anonymous")
            } else {
                crate::tr!(
                    "pull_requests.files.viewed_owner_tcode_tooltip",
                    host_name = self.host_name(cx)
                )
            }
            .into_owned();
            let mut counter = crate::tr!(
                "pull_requests.files.viewed_count",
                viewed = viewed.to_string(),
                total = total.to_string()
            )
            .into_owned();
            // The host keeps no marks Tcode can read, so the ones shown are Tcode's own.
            if !host_marks {
                counter.push_str(" · ");
                counter.push_str(&crate::tr!("pull_requests.files.viewed_owner_tcode"));
            }
            div()
                .id("pr-viewed-counter")
                .flex_none()
                .text_secondary(compact, cx)
                .child(counter)
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        });
        let split = self.store.read(cx).diff_split();
        let wrap = self.store.read(cx).diff_word_wrap();
        let all_collapsed = files.is_some_and(|files| {
            !files.files.is_empty() && files.files.iter().all(|file| self.collapsed(&file.path))
        });
        let options = [
            (
                FilesOption::Split,
                "pr-files-split",
                IconName::PanelLeft,
                split,
                if split {
                    crate::tr!("diff.unified_view")
                } else {
                    crate::tr!("diff.split_view")
                }
                .into_owned(),
            ),
            (
                FilesOption::Wrap,
                "pr-files-wrap",
                IconName::Menu,
                wrap,
                crate::tr!("diff.toggle_wrap").into_owned(),
            ),
            (
                FilesOption::Whitespace,
                "pr-files-whitespace",
                IconName::Eye,
                self.ignore_ws,
                crate::tr!("diff.toggle_whitespace").into_owned(),
            ),
            (
                FilesOption::Invisibles,
                "pr-files-invisibles",
                IconName::CaseSensitive,
                self.show_invisibles,
                crate::tr!("diff.toggle_invisibles").into_owned(),
            ),
        ];
        let mut toolbar = material::bar("pr-files-toolbar", material::Bar::Toolbar, compact, cx)
            .items_center()
            .when(!wide_column, |toolbar| {
                toolbar.child(self.file_jump(count_label, cx))
            })
            .children(viewed_counter)
            .children(self.review_entry(cx))
            .child(div().flex_1());
        if compact {
            let can_start_review = self.can_review(cx) && self.draft(cx).is_none();
            toolbar = toolbar.child(
                material::toolbar_icon_button(
                    "pr-files-options",
                    IconName::Ellipsis,
                    crate::tr!("mobile.more_actions"),
                    true,
                )
                .dropdown_menu(move |mut menu, _, _| {
                    if can_start_review {
                        menu = menu
                            .menu(
                                crate::tr!("pull_requests.review.entry_menu").into_owned(),
                                Box::new(FilesOption::Review),
                            )
                            .separator();
                    }
                    for (option, _, _, on, label) in &options {
                        menu = menu.menu_with_check(label.clone(), *on, Box::new(*option));
                    }
                    menu.separator().menu(
                        crate::tr!(if all_collapsed {
                            "pull_requests.files.expand_all"
                        } else {
                            "pull_requests.files.collapse_all"
                        })
                        .into_owned(),
                        Box::new(if all_collapsed {
                            FilesOption::ExpandAll
                        } else {
                            FilesOption::CollapseAll
                        }),
                    )
                }),
            );
        } else {
            for (option, id, icon, on, label) in options {
                toolbar = toolbar.child(
                    Button::new(id)
                        .ghost()
                        .small()
                        .compact()
                        .icon(icon)
                        .selected(on)
                        .tooltip(label)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.apply_option(option, window, cx)
                        })),
                );
            }
            toolbar = toolbar
                .child(
                    Button::new("pr-files-collapse")
                        .ghost()
                        .small()
                        .compact()
                        .icon(if all_collapsed {
                            IconName::ChevronsUpDown
                        } else {
                            IconName::ChevronsDownUp
                        })
                        .tooltip(if all_collapsed {
                            crate::tr!("pull_requests.files.expand_all")
                        } else {
                            crate::tr!("pull_requests.files.collapse_all")
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_all_collapsed(!all_collapsed, cx)
                        })),
                )
                .child(
                    Button::new("pr-files-column")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::PanelLeft)
                        .selected(self.file_column)
                        .tooltip(if self.file_column {
                            crate::tr!("pull_requests.files.hide_list")
                        } else {
                            crate::tr!("pull_requests.files.show_list")
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.file_column = !this.file_column;
                            cx.notify();
                        })),
                );
        }
        toolbar.into_any_element()
    }

    /// The files matching the filter, in the host's order.
    fn filtered(&self, filter: &str) -> Vec<usize> {
        let filter = filter.to_lowercase();
        self.files_of()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .filter(|(_, file)| filter.is_empty() || file.path.to_lowercase().contains(&filter))
            .map(|(index, _)| index)
            .collect()
    }

    /// The files as a virtual list, so a three-thousand-file pull request lays out one screen.
    fn file_list(
        &self,
        id: &'static str,
        filter: &str,
        in_popover: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let indexes = self.filtered(filter);
        let view = cx.entity();
        crate::scroll::VirtualList::uniform(id, indexes.len(), move |range, _, cx| {
            view.update(cx, |view, cx| {
                range
                    .map(|position| view.file_row(indexes[position], in_popover, cx))
                    .collect()
            })
        })
        .w_full()
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn file_row(&self, index: usize, in_popover: bool, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let Some(file) = self.files_of().and_then(|files| files.get(index)).cloned() else {
            return div().into_any_element();
        };
        let top = self
            .page()
            .and_then(|page| page.files_view.list.as_ref())
            .and_then(|list| list.top_file(self.store.read(cx).diff_split()));
        let muted = cx.theme().muted_foreground;
        let (directory, name) = match file.path.rsplit_once('/') {
            Some((directory, name)) => (format!("{directory}/"), name.to_owned()),
            None => (String::new(), file.path.clone()),
        };
        let availability = match file.patch {
            PullRequestPatch::Binary => Some((IconName::Binary, muted)),
            PullRequestPatch::Oversized | PullRequestPatch::Withheld => {
                Some((IconName::FileX, muted))
            }
            PullRequestPatch::Hunks(_) => None,
        };
        let tooltip = file
            .previous_path
            .as_ref()
            .map(|previous| format!("{previous} → {}", file.path))
            .unwrap_or_else(|| file.path.clone());
        material::selectable_row(
            material::accessible_clickable(
                h_flex(),
                SharedString::from(format!("pr-file-row-{in_popover}-{index}")),
                gpui::Role::Button,
                file.path.clone(),
                cx,
            ),
            top == Some(index) && !in_popover,
            compact,
            cx,
        )
        .w_full()
        .text_token(&cx.theme().tokens.typography.xs)
        .cursor_pointer()
        .child(Icon::new(IconName::File).text_color(kind_rail(file.kind, cx).unwrap_or(muted)))
        // gpui-base has no start truncation: the directory gives way first, then the name.
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(
                    div()
                        .flex_shrink(8.)
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(muted)
                        .child(crate::widgets::fade_text(directory)),
                )
                .child(
                    div()
                        .flex_shrink(1.)
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(name)),
                ),
        )
        .children(availability.map(|(icon, color)| Icon::new(icon).text_color(color)))
        .child(
            h_flex()
                .flex_none()
                .gap_1()
                .text_meta_mono(false, cx)
                .child(
                    div()
                        .text_color(cx.theme().success)
                        .child(format!("+{}", file.additions)),
                )
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(format!("−{}", file.deletions)),
                ),
        )
        .children(
            self.viewed_box(index, false, cx)
                .filter(|_| !in_popover || compact),
        )
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.scroll_to(index, cx);
            if in_popover {
                window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
            }
        }))
        .into_any_element()
    }

    fn filter_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some((input, _)) = &self.file_filter {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        let subscription = cx.observe(&input, |_, _, cx| cx.notify());
        self.file_filter = Some((input.clone(), subscription));
        input
    }

    fn file_jump(&self, label: String, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let muted = cx.theme().muted_foreground;
        let view = cx.entity();
        let trigger = Button::new("pr-file-jump")
            .ghost()
            .outline()
            .compact()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_body(false, cx)
                    .child(Icon::new(IconName::Files))
                    .child(label)
                    .child(Icon::new(IconName::ChevronDown).text_color(muted)),
            );
        let popover = Popover::new("pr-file-jump-popover").trigger(trigger);
        let popover = if compact {
            popover.bottom_sheet(crate::tr!("pull_requests.detail.files").into_owned())
        } else {
            popover
        };
        popover
            .content(move |_, window, cx| {
                view.update(cx, |view, cx| {
                    let input = view.filter_input(window, cx);
                    let filter = input.read(cx).value().to_string();
                    v_flex()
                        .w(px(360.))
                        .max_h(px(420.))
                        .gap_1()
                        .child(
                            Input::new(&input)
                                .placeholder(crate::tr!("pull_requests.files.filter")),
                        )
                        .child(view.file_list("pr-file-jump-rows", &filter, true, cx))
                })
            })
            .into_any_element()
    }

    fn file_column(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let input = self.filter_input(window, cx);
        let filter = input.read(cx).value().to_string();
        let count = self.count_label();
        v_flex()
            .flex_none()
            .w(px(280.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .p_1()
            .gap_1()
            .child(div().px_2().text_secondary(false, cx).child(count))
            .child(Input::new(&input).placeholder(crate::tr!("pull_requests.files.filter")))
            .child(self.file_list("pr-file-column-rows", &filter, false, cx))
            .into_any_element()
    }

    pub(super) fn render_files(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_list(cx);
        self.apply_text(cx);
        if let Some(failure) = self
            .page_mut()
            .and_then(|page| page.files_view.text_failure.take())
        {
            window.push_notification(Notification::warning(failure), cx);
        }
        let Some(page) = self.page() else {
            return div().into_any_element();
        };
        let files = page.files.data.as_ref();
        if files.is_none() {
            return match &page.files.error {
                Some(error) => {
                    let error = error.clone();
                    self.failure(
                        &error,
                        crate::tr!("pull_requests.files.load_failed").into_owned(),
                        cx,
                    )
                }
                None => material::loading_skeleton(cx),
            };
        }
        let files = files.cloned().unwrap_or_else(|| unreachable!());
        if files.files.is_empty() && files.next_cursor.is_none() {
            return material::empty_state(
                Icon::new(IconName::File),
                crate::tr!("pull_requests.files.empty_title").into_owned(),
                crate::tr!("pull_requests.files.empty_desc").into_owned(),
                cx,
            )
            .into_any_element();
        }
        // The next page is read as the reader nears the end of what is drawn.
        let split = self.store.read(cx).diff_split();
        let wrap = self.store.read(cx).diff_word_wrap();
        let near_end = page
            .files_view
            .list
            .as_ref()
            .and_then(|list| list.top_file(split))
            .is_some_and(|top| top + 20 >= files.files.len());
        if let Some(next) = files.next_cursor.clone()
            && near_end
            && !page.files_view.page_loading
            && page.files_view.page_error.is_none()
        {
            self.read_next_page(next, cx);
        }
        let Some(page) = self.page() else {
            return div().into_any_element();
        };
        let compact = self.compact(cx);
        let wide = !compact
            && f32::from(window.viewport_size().width) >= FILE_COLUMN_MIN_WIDTH
            && self.file_column;
        let partial = (files.next_cursor.is_none() && !files.complete).then(|| {
            let listed = files.files.len();
            let text = if files.changed_files > listed as u64 {
                crate::tr!(
                    "pull_requests.files.partial",
                    shown = listed.to_string(),
                    total = files.changed_files.to_string(),
                    host_name = self.host_name(cx)
                )
            } else {
                crate::tr!("pull_requests.files.partial_unknown")
            }
            .into_owned();
            div().mx_2().mb_1().child(render_notice(text, cx))
        });
        // Keep the paging footer until prepared rows are appended, so the old
        // list cannot clamp to a larger viewport before the new page exists.
        let footer = (page.files_view.page_loading
            || (page.files_view.rendering && page.files_view.list.is_some())
            || page.files_view.page_error.is_some())
        .then(|| {
            let failed = page.files_view.page_error.is_some();
            material::bar("pr-files-footer", material::Bar::Toolbar, compact, cx)
                .items_center()
                .text_secondary(compact, cx)
                .when(!failed, |row| {
                    row.child(crate::widgets::spinner::Spinner::new())
                        .child(crate::tr!("pull_requests.files.loading_more"))
                })
                .when(failed, |row| {
                    row.child(crate::tr!("pull_requests.files.rest_failed"))
                        .child(
                            Button::new("pr-files-rest-retry")
                                .outline()
                                .xsmall()
                                .label(crate::tr!("pull_requests.detail.retry"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(page) = this.page_mut() {
                                        page.files_view.page_error = None;
                                    }
                                    cx.notify();
                                })),
                        )
                })
        });
        let body = if page.files_view.list.is_some() {
            render_list(self, "pr-files-body", split, wrap, window, cx)
        } else {
            material::loading_skeleton(cx)
        };
        let off_diff = self.off_diff(cx);
        let content = v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .children(partial)
            .children(off_diff)
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .when(compact, |body| body.px(px(material::COMPACT_PAGE_INSET)))
                    .child(body),
            )
            .children(footer);
        let review_bar = self.review_bar(cx);
        v_flex()
            .size_full()
            .min_h_0()
            .on_action(cx.listener(Self::on_option))
            .child(self.toolbar(wide, cx))
            .children(review_bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .when(wide, |row| row.child(self.file_column(window, cx)))
                    .child(content),
            )
            .into_any_element()
    }

    fn file_notice(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.files_of().and_then(|files| files.get(file)).cloned() else {
            return div().into_any_element();
        };
        let (icon, text) = match entry.patch {
            PullRequestPatch::Binary => (
                IconName::Binary,
                crate::tr!("pull_requests.files.binary").into_owned(),
            ),
            PullRequestPatch::Oversized => (
                IconName::FileX,
                crate::tr!(
                    "pull_requests.files.oversized",
                    size = crate::tr!(
                        "pull_requests.files.line_changes",
                        count = (entry.additions + entry.deletions).to_string()
                    )
                    .into_owned()
                )
                .into_owned(),
            ),
            PullRequestPatch::Withheld => (
                IconName::FileX,
                crate::tr!(
                    "pull_requests.files.withheld",
                    host_name = self.host_name(cx)
                )
                .into_owned(),
            ),
            PullRequestPatch::Hunks(_) => return div().into_any_element(),
        };
        let url = self.file_url(&entry.path, cx);
        // A file with no patch keeps a block about a short hunk's height in the list; no
        // owner carries a placeholder height.
        material::card(cx)
            .flex_row()
            .min_w_full()
            .min_h(px(72.))
            .my_1()
            .items_center()
            .text_secondary(false, cx)
            .font_family(cx.theme().font_family.clone())
            .child(Icon::new(icon))
            .child(div().flex_1().min_w_0().child(text))
            .children(url.map(|url| {
                let host_name = self.host_name(cx);
                Button::new(SharedString::from(format!("pr-file-open-{file}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ExternalLink)
                    .label(crate::tr!(
                        "pull_requests.open_on_host",
                        host_name = &host_name
                    ))
                    .on_click(move |_, _, cx| cx.open_url(&url))
            }))
            .into_any_element()
    }
}

impl DiffListHost for PullRequestView {
    fn diff_list(&self) -> Option<&DiffList> {
        self.page()?.files_view.list.as_ref()
    }

    fn diff_list_mut(&mut self) -> Option<&mut DiffList> {
        let current = self.current.clone()?;
        self.pages.get_mut(&current)?.files_view.list.as_mut()
    }

    fn expand_gap(
        &mut self,
        file: usize,
        lines: Range<u32>,
        direction: ExpandDir,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self
            .files_of()
            .and_then(|files| files.get(file))
            .map(|file| file.path.clone())
        else {
            return;
        };
        let loaded = matches!(
            self.page()
                .and_then(|page| page.files_view.texts.get(&path)),
            Some(TextState::Loaded { .. })
        );
        if loaded {
            if let Some(list) = self.diff_list_mut() {
                list.expand(file, lines, direction);
            }
        } else if let Some(page) = self.page_mut()
            && !page.files_view.texts.contains_key(&path)
        {
            page.files_view.pending_expand = Some((path, lines.start, direction));
            self.read_text(file, cx);
        }
        cx.notify();
    }

    fn gap_state(&self, file: usize, expandable: bool) -> GapState {
        let Some(entry) = self.files_of().and_then(|files| files.get(file)) else {
            return GapState::Fixed(None);
        };
        let head = self.head().unwrap_or_default();
        let short = head.get(..7).unwrap_or(&head).to_owned();
        match self
            .page()
            .and_then(|page| page.files_view.texts.get(&entry.path))
        {
            Some(TextState::Loading) => GapState::Loading,
            Some(TextState::Loaded { .. }) if expandable => GapState::Expandable,
            Some(TextState::Loaded { .. }) | Some(TextState::Unreadable) => GapState::Fixed(None),
            Some(TextState::Missing) => GapState::Fixed(Some(
                crate::tr!("pull_requests.files.side_missing", revision = short).into_owned(),
            )),
            Some(TextState::Oversized) => GapState::Fixed(Some(
                crate::tr!("pull_requests.files.too_large_to_expand").into_owned(),
            )),
            None => GapState::Expandable,
        }
    }

    fn header_title(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.files_of().and_then(|files| files.get(file)) else {
            return div().into_any_element();
        };
        h_flex()
            .min_w_0()
            .gap_1()
            .text_body(false, cx)
            .font_weight(gpui::FontWeight::MEDIUM)
            .children(entry.previous_path.as_ref().map(|previous| {
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::widgets::fade_text(format!("{previous} →")))
            }))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(cx.theme().foreground)
                    .child(crate::widgets::fade_text(entry.path.clone())),
            )
            .into_any_element()
    }

    fn header_leading(&self, file: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.files_of()?.get(file)?.path.clone();
        let collapsed = self.collapsed(&path);
        Some(
            div()
                .id(SharedString::from(format!("pr-file-chevron-{file}")))
                .flex_none()
                .child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .text_color(cx.theme().muted_foreground),
                )
                .tooltip(move |window, cx| {
                    Tooltip::new(
                        crate::tr!(if collapsed {
                            "pull_requests.files.expand_file"
                        } else {
                            "pull_requests.files.collapse_file"
                        })
                        .into_owned(),
                    )
                    .build(window, cx)
                })
                .into_any_element(),
        )
    }

    fn header_note(&self, file: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = &self.files_of()?.get(file)?.path;
        (self.viewed_state(path) == Viewed::Changed).then(|| {
            div()
                .text_secondary(false, cx)
                .text_color(cx.theme().warning)
                .child(crate::tr!("pull_requests.files.changed_since_viewed"))
                .into_any_element()
        })
    }

    fn header_trailing(&self, file: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.viewed_box(file, true, cx)
    }

    fn header_clicked(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(path) = self
            .files_of()
            .and_then(|files| files.get(file))
            .map(|file| file.path.clone())
        else {
            return;
        };
        let collapsed = self.collapsed(&path);
        if let Some(page) = self.page_mut() {
            page.files_view.collapsed.insert(path, !collapsed);
        }
        cx.notify();
    }

    fn file_menu(&self, file: usize, cx: &App) -> FileMenu {
        let path = self
            .files_of()
            .and_then(|files| files.get(file))
            .map(|file| file.path.clone())
            .unwrap_or_default();
        let url = self.file_url(&path, cx);
        let host_name = self.host_name(cx);
        Rc::new(move |menu, _, _| {
            menu.menu(
                crate::tr!("pull_requests.files.copy_path").into_owned(),
                Box::new(CopyText(path.clone())),
            )
            .when_some(url.clone(), |menu, url| {
                menu.menu(
                    crate::tr!("pull_requests.open_on_host", host_name = &host_name).into_owned(),
                    Box::new(OpenUrl(url)),
                )
            })
        })
    }

    fn row_extras(
        &self,
        file: usize,
        old: Option<u32>,
        new: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(path) = self
            .files_of()
            .and_then(|files| files.get(file))
            .map(|file| file.path.clone())
        else {
            return Vec::new();
        };
        let threads: Vec<_> = self
            .inline_threads()
            .into_iter()
            .filter(|thread| {
                thread.anchor.as_ref().is_some_and(|anchor| {
                    anchor.path == path
                        && match anchor.side {
                            ReviewSide::Old => old == Some(anchor.end_line),
                            ReviewSide::New => new == Some(anchor.end_line),
                        }
                })
            })
            .cloned()
            .collect();
        let mut extras: Vec<AnyElement> = threads
            .iter()
            .map(|thread| self.thread_card(thread, true, cx))
            .collect();
        extras.extend(self.review_row_extras(&path, old, new, cx));
        extras
    }

    fn review_comment_menu(&self, cx: &App) -> Option<(bool, SharedString)> {
        self.line_comment_menu(cx)
    }

    fn notice(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        self.file_notice(file, cx)
    }
}

/// One file of the pull request through the diff renderer: the hunks alone until its text at
/// the head has been read, then the whole file.
fn render(
    file: &PullRequestFile,
    text: Option<&(String, String)>,
    ignore_whitespace: bool,
    show_invisibles: bool,
    theme: &crate::highlight::HighlightTheme,
    colors: &DiffColors,
    whitespace: &HighlightStyle,
) -> RenderedFile {
    let patch = match &file.patch {
        PullRequestPatch::Hunks(hunks) => Some(hunks.as_str()),
        _ => None,
    };
    let mut rendered = build_file(
        &FileDiffInput {
            path: &file.path,
            kind: file.kind,
            old_text: text.map(|(old, _)| old.as_str()),
            new_text: text.map(|(_, new)| new.as_str()),
            patch,
            ignore_whitespace,
            show_invisibles,
        },
        file.path.clone(),
        crate::highlight::language_name_for_path(&file.path),
        theme,
        colors,
        whitespace,
    );
    if !matches!(file.patch, PullRequestPatch::Hunks(_)) {
        rendered.added = file.additions;
        rendered.removed = file.deletions;
    }
    rendered
}

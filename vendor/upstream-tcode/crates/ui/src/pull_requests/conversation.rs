//! The Conversation view: the description, comments, reviews and review threads, oldest first.

use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, list, prelude::FluentBuilder as _, px,
};
use gpui_base::{Avatar, AvatarFallback, AvatarImage, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_core::{pull_request::PullRequestState, session::ReviewSide};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult, PullRequestCapabilities, PullRequestComment,
    PullRequestReaction, PullRequestReactionContent, PullRequestRead, PullRequestReadResponse,
    PullRequestReviewState, PullRequestReviewThread,
};

use super::compose::{EditorSpec, Sheet, Slot, Write};
use super::detail::{PullRequestView, Replies, Tab, ago_rfc3339, reason};
use crate::{
    icon::{Icon, IconName},
    markdown::{ImageResolver, MarkdownState, MarkdownView},
    material,
    sizing::Sizable as _,
    store::{MediaState, pull_request_media, pull_request_media_state},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Popover,
        button::{Button, ButtonVariants as _},
        menu::{CopyText, DropdownMenu as _, OpenUrl},
        tooltip::Tooltip,
    },
};

/// A comment's ⋯ menu item that writes.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = tcode_pull_requests, no_json)]
pub(super) enum CommentMenu {
    Edit {
        id: String,
    },
    EditDescription,
    /// Quotes the comment into the composer, or into its thread's reply.
    Quote {
        id: String,
        thread: Option<String>,
    },
    React {
        id: String,
    },
}

/// One row of the conversation list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Notice,
    Meta,
    Description,
    Comment(usize),
    Thread(usize),
    /// Merged or closed, from the thread's snapshot of the pull request.
    Event(PullRequestState, String),
    PendingReview,
    Composer,
}

/// The body without HTML comments, which GitHub templates leave behind; `None` when nothing
/// is left to show.
pub(super) fn visible_body(body: &str) -> Option<String> {
    let mut visible = String::new();
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        visible.push_str(&rest[..start]);
        rest = rest[start..]
            .find("-->")
            .map_or("", |end| &rest[start + end + 3..]);
    }
    visible.push_str(rest);
    let visible = visible.trim().to_owned();
    (!visible.is_empty()).then_some(visible)
}

fn reaction_name(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "pull_requests.reactions.thumbs_up",
        PullRequestReactionContent::ThumbsDown => "pull_requests.reactions.thumbs_down",
        PullRequestReactionContent::Laugh => "pull_requests.reactions.laugh",
        PullRequestReactionContent::Hooray => "pull_requests.reactions.hooray",
        PullRequestReactionContent::Confused => "pull_requests.reactions.confused",
        PullRequestReactionContent::Heart => "pull_requests.reactions.heart",
        PullRequestReactionContent::Rocket => "pull_requests.reactions.rocket",
        PullRequestReactionContent::Eyes => "pull_requests.reactions.eyes",
    }
}

fn reaction_emoji(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "👍",
        PullRequestReactionContent::ThumbsDown => "👎",
        PullRequestReactionContent::Laugh => "😄",
        PullRequestReactionContent::Hooray => "🎉",
        PullRequestReactionContent::Confused => "😕",
        PullRequestReactionContent::Heart => "❤️",
        PullRequestReactionContent::Rocket => "🚀",
        PullRequestReactionContent::Eyes => "👀",
    }
}

/// A reaction chip; the account's own reactions show selected.
fn reaction_chip(id: SharedString, label: String, own: bool, cx: &App) -> Button {
    Button::new(id)
        .ghost()
        .outline()
        .small()
        .rounded(material::radius_chip(cx))
        .selected(own)
        .aria_label(label)
        .text_token(&cx.theme().tokens.typography.xs)
}

impl PullRequestView {
    fn items(&self, cx: &App) -> Vec<Item> {
        let Some(conversation) = self.page().and_then(|page| page.conversation.data.as_ref())
        else {
            return Vec::new();
        };
        let mut timed: Vec<(String, Item)> = Vec::new();
        for (index, comment) in conversation.comments.iter().enumerate() {
            if visible_body(&comment.body).is_some() || comment.review_state.is_some() {
                timed.push((comment.created_at.clone(), Item::Comment(index)));
            }
        }
        for (index, thread) in conversation.threads.iter().enumerate() {
            let at = thread
                .comments
                .first()
                .map(|comment| comment.created_at.clone())
                .unwrap_or_default();
            timed.push((at, Item::Thread(index)));
        }
        if let Some(snapshot) = self.link(cx).and_then(|link| link.snapshot) {
            match snapshot.state {
                PullRequestState::Merged => {
                    if let Some(at) = snapshot.merged_at {
                        timed.push((at.clone(), Item::Event(PullRequestState::Merged, at)));
                    }
                }
                PullRequestState::Closed => {
                    if let Some(at) = snapshot.closed_at {
                        timed.push((at.clone(), Item::Event(PullRequestState::Closed, at)));
                    }
                }
                PullRequestState::Open => {}
            }
        }
        timed.sort_by(|left, right| left.0.cmp(&right.0));
        let mut items = Vec::with_capacity(timed.len() + 5);
        if !conversation.complete {
            items.push(Item::Notice);
        }
        items.push(Item::Meta);
        items.push(Item::Description);
        items.extend(timed.into_iter().map(|(_, item)| item));
        if self.draft(cx).is_some() {
            items.push(Item::PendingReview);
        }
        items.push(Item::Composer);
        items
    }

    pub(super) fn scroll_conversation_to_end(&mut self, cx: &mut Context<Self>) {
        let count = self.items(cx).len();
        if let Some(page) = self.page_mut()
            && count > 0
        {
            if page.conversation_view.list.item_count() != count {
                page.conversation_view.list.reset(count);
            }
            page.conversation_view.list.scroll_to(gpui::ListOffset {
                item_ix: count - 1,
                offset_in_item: px(0.),
            });
        }
    }

    /// Whether this device may write here at all: not read-only, and not waiting for the read
    /// after an unanswered write.
    fn writable(&self, cx: &App) -> bool {
        !self.read_only(cx) && !self.writes().is_some_and(|writes| writes.waiting)
    }

    pub(super) fn on_comment_menu(
        &mut self,
        action: &CommentMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            CommentMenu::Edit { id } => {
                let body = self.comment_body(id).unwrap_or_default();
                self.open_editor(
                    Slot::Edit(id.clone()),
                    crate::tr!("pull_requests.compose.comment_placeholder").into(),
                    body.clone(),
                    Some(body),
                    window,
                    cx,
                );
            }
            CommentMenu::EditDescription => {
                let body = self
                    .page()
                    .and_then(|page| page.conversation.data.as_ref())
                    .map(|conversation| conversation.description.body.clone())
                    .unwrap_or_default();
                self.open_editor(
                    Slot::Description,
                    crate::tr!("pull_requests.compose.comment_placeholder").into(),
                    body.clone(),
                    Some(body),
                    window,
                    cx,
                );
            }
            CommentMenu::Quote { id, thread } => {
                let body = self.comment_body(id).unwrap_or_default();
                let quote = body
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n\n";
                let slot = thread.clone().map_or(Slot::Composer, Slot::Reply);
                let held = self
                    .writes()
                    .and_then(|writes| writes.editors.get(&slot))
                    .map(|editor| editor.input.clone());
                match held {
                    Some(input) => input.update(cx, |input, cx| {
                        let text = input.value().to_string();
                        let joined = if text.trim().is_empty() {
                            quote.clone()
                        } else {
                            format!("{text}\n\n{quote}")
                        };
                        input.set_value(joined, window, cx);
                        input.focus(window, cx);
                    }),
                    None => self.open_editor(
                        slot.clone(),
                        crate::tr!(if thread.is_some() {
                            "pull_requests.compose.reply_placeholder"
                        } else {
                            "pull_requests.compose.comment_placeholder"
                        })
                        .into(),
                        quote,
                        None,
                        window,
                        cx,
                    ),
                }
                if self.compact(cx) {
                    self.open_sheet(Some(Sheet::Editor(slot)), cx);
                } else if thread.is_none() {
                    self.scroll_conversation_to_end(cx);
                }
            }
            CommentMenu::React { id } => self.open_sheet(Some(Sheet::Reactions(id.clone())), cx),
        }
        if let Some(page) = self.page() {
            page.conversation_view.list.remeasure();
        }
        cx.notify();
    }

    /// Adds or takes back the account's reaction, shown at once and corrected by the next read.
    fn react(
        &mut self,
        subject: String,
        content: PullRequestReactionContent,
        reacted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(writes) = self.writes_mut() {
            writes.reactions.insert((subject.clone(), content), reacted);
            writes.sheet = None;
        }
        let key = (subject.clone(), content);
        self.send_write(
            PullRequestAction::React {
                subject_id: subject.clone(),
                content,
                reacted,
            },
            Write::Reaction,
            format!("react {subject} {content:?}"),
            window,
            cx,
            move |this, result, _, cx| {
                if matches!(result, PullRequestActionResult::Rejected(_))
                    && let Some(writes) = this.writes_mut()
                {
                    writes.reactions.remove(&key);
                }
                cx.notify();
            },
        );
    }

    fn resolve(
        &mut self,
        thread: String,
        resolved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = thread.clone();
        self.send_write(
            PullRequestAction::ResolveThread {
                thread_id: thread.clone(),
                resolved,
            },
            Write::Resolve(resolved),
            format!("resolve {thread}"),
            window,
            cx,
            move |this, result, _, cx| {
                if *result == PullRequestActionResult::Applied
                    && let Some(page) = this.page_mut()
                {
                    // A4's collapse rule: resolved folds away unless the reader opens it.
                    if resolved {
                        page.conversation_view.expanded.remove(&id);
                    } else {
                        page.conversation_view.expanded.insert(id.clone());
                    }
                    page.conversation_view.list.remeasure();
                }
                cx.notify();
            },
        );
    }

    /// Switches to the conversation at a thread.
    pub(super) fn show_thread(&mut self, id: &str, cx: &mut Context<Self>) {
        let items = self.items(cx);
        let Some(page) = self.page_mut() else { return };
        page.tab = Some(Tab::Conversation);
        let position = items.iter().position(|item| match item {
            Item::Thread(index) => page
                .conversation
                .data
                .as_ref()
                .and_then(|conversation| conversation.threads.get(*index))
                .is_some_and(|thread| thread.id == id),
            _ => false,
        });
        page.conversation_view.expanded.insert(id.to_owned());
        if let Some(position) = position {
            if page.conversation_view.list.item_count() != items.len() {
                page.conversation_view.list.reset(items.len());
            }
            page.conversation_view.list.scroll_to(gpui::ListOffset {
                item_ix: position,
                offset_in_item: px(0.),
            });
        }
        cx.notify();
    }

    fn show_in_files(&mut self, thread: &PullRequestReviewThread, cx: &mut Context<Self>) {
        let Some(anchor) = thread.anchor.clone() else {
            return;
        };
        let index = self
            .files_of()
            .and_then(|files| files.iter().position(|file| file.path == anchor.path));
        if let Some(page) = self.page_mut() {
            page.tab = Some(Tab::Files);
            page.files_view.collapsed.insert(anchor.path.clone(), false);
            if let (Some(index), Some(list)) = (index, page.files_view.list.as_ref()) {
                list.scroll_to_file(index);
            }
        }
        cx.notify();
    }

    /// Every image the conversation shows goes to the host, which decides how it is read.
    fn resolver(&self) -> Option<ImageResolver> {
        let (session, key) = self.current.clone()?;
        let account = self.page()?.conversation.data.as_ref()?.account.clone();
        let source = {
            let (session, key, account) = (session.clone(), key.clone(), account.clone());
            Rc::new(move |url: &str| {
                Some(pull_request_media(
                    session.clone(),
                    key.clone(),
                    account.clone(),
                    url.to_owned(),
                ))
            })
        };
        let store = self.store.clone();
        let pending = Rc::new(
            move |url: &str,
                  title: &str,
                  window: &mut Window,
                  cx: &mut App|
                  -> Option<AnyElement> {
                let state = pull_request_media_state(&session, &key, &account, url, window, cx);
                let host_name = super::host_name(store.read(cx), &key.host);
                media_stand_in(state, url, title, &host_name, cx)
            },
        );
        Some(ImageResolver { source, pending })
    }

    /// The comment's rendered body, kept per comment so its selection and layout survive
    /// re-reads; a conversation read as another account gets fresh ones.
    pub(super) fn markdown(
        &self,
        id: &str,
        body: &str,
        cx: &mut Context<Self>,
    ) -> Entity<MarkdownState> {
        let resolver = self.resolver();
        let account = self
            .page()
            .and_then(|page| page.conversation.data.as_ref())
            .map(|conversation| conversation.account.clone())
            .unwrap_or_default();
        let Some(page) = self.page() else {
            return cx.new(|cx| MarkdownState::new(body, cx));
        };
        let held = page
            .conversation_view
            .markdown
            .borrow()
            .get(id)
            .filter(|(held_account, _)| *held_account == account)
            .map(|(_, state)| state.clone());
        if let Some(state) = held {
            state.update(cx, |state, cx| state.set_text(body, cx));
            return state;
        }
        let state = cx.new(|cx| {
            let mut state = MarkdownState::new(body, cx);
            state.set_selectable(true, cx);
            state.set_compact_headings(true, cx);
            state.set_image_resolver(resolver, cx);
            state
        });
        page.conversation_view
            .markdown
            .borrow_mut()
            .insert(id.to_owned(), (account, state.clone()));
        state
    }

    pub(super) fn avatar(&self, login: &str, url: Option<&str>, size: f32, cx: &App) -> AnyElement {
        let initial = login
            .chars()
            .next()
            .map(|initial| initial.to_uppercase().to_string())
            .unwrap_or_default();
        let fallback = Avatar::new()
            .size(px(size))
            .rounded_full()
            .bg(cx.theme().muted)
            .fallback(
                AvatarFallback::new()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    // The initial scales with the avatar, which no text token does.
                    .text_size(px(size * 0.5))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(initial),
            );
        let image = self
            .current
            .as_ref()
            .zip(url)
            .and_then(|((session, key), url)| {
                let account = self.page()?.conversation.data.as_ref()?.account.clone();
                Some(
                    Avatar::new()
                        .absolute()
                        .inset_0()
                        .rounded_full()
                        .overflow_hidden()
                        .image(
                            AvatarImage::new(pull_request_media(
                                session.clone(),
                                key.clone(),
                                account,
                                url.to_owned(),
                            ))
                            .size(px(size))
                            .rounded_full(),
                        ),
                )
            });
        // The initial stands behind the picture until it loads, and instead of it on failure.
        div()
            .relative()
            .flex_none()
            .size(px(size))
            .child(fallback)
            .children(image)
            .into_any_element()
    }

    fn comment_head(
        &self,
        comment: &PullRequestComment,
        action: Option<AnyElement>,
        size: f32,
        place: Place,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let login = comment
            .author
            .as_ref()
            .map(|author| author.login.clone())
            .unwrap_or_else(|| crate::tr!("pull_requests.conversation.ghost").into_owned());
        let avatar = self.avatar(
            &login,
            comment
                .author
                .as_ref()
                .and_then(|author| author.avatar_url.as_deref()),
            size,
            cx,
        );
        let created = comment.created_at.clone();
        let url = comment.url.clone();
        let body = comment.body.clone();
        let writable = self.writable(cx);
        let update = self
            .page()
            .and_then(|page| page.conversation.data.as_ref())
            .is_some_and(|conversation| conversation.permissions.update);
        let mut writes: Vec<(&'static str, CommentMenu)> = Vec::new();
        if writable {
            if place.description {
                if update {
                    writes.push((
                        "pull_requests.compose.edit_description",
                        CommentMenu::EditDescription,
                    ));
                }
            } else if comment.viewer_can_update && comment.review_state.is_none() {
                writes.push((
                    "pull_requests.compose.edit",
                    CommentMenu::Edit {
                        id: comment.id.clone(),
                    },
                ));
            }
            writes.push((
                "pull_requests.compose.quote_reply",
                CommentMenu::Quote {
                    id: comment.id.clone(),
                    thread: place.thread.clone(),
                },
            ));
            if self.can_react(comment) {
                writes.push((
                    "pull_requests.reactions.add_menu",
                    CommentMenu::React {
                        id: comment.id.clone(),
                    },
                ));
            }
        }
        // With no reactions yet, the add chip lives in the head row.
        let add_reaction =
            (writable && self.can_react(comment) && self.shown_reactions(comment).is_empty())
                .then(|| self.reaction_popover(comment, true, cx));
        h_flex()
            .group(SharedString::from(format!(
                "pr-comment-head-{}",
                comment.id
            )))
            .gap_2()
            .items_center()
            .text_secondary(self.compact(cx), cx)
            .child(avatar)
            .child(
                div()
                    .font_medium()
                    .text_color(cx.theme().foreground)
                    .child(login),
            )
            .children(action)
            .child(
                div()
                    .id(SharedString::from(format!("pr-ago-{}", comment.id)))
                    .text_color(muted)
                    .children(ago_rfc3339(&comment.created_at))
                    .tooltip(move |window, cx| {
                        let local = chrono::DateTime::parse_from_rfc3339(&created)
                            .map(|time| {
                                time.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M")
                                    .to_string()
                            })
                            .unwrap_or_default();
                        Tooltip::new(local).build(window, cx)
                    }),
            )
            .when(comment.edited_at.is_some(), |head| {
                head.child(div().child(crate::tr!("pull_requests.conversation.edited")))
            })
            .child(div().flex_1())
            .children(add_reaction)
            .child({
                let host_name = self.host_name(cx);
                Button::new(SharedString::from(format!(
                    "pr-comment-menu-{}",
                    comment.id
                )))
                .ghost()
                .xsmall()
                .compact()
                .icon(IconName::Ellipsis)
                .dropdown_menu(move |mut menu, _, _| {
                    for (label, action) in &writes {
                        menu = menu.menu(crate::tr!(label).into_owned(), Box::new(action.clone()));
                    }
                    if !writes.is_empty() {
                        menu = menu.separator();
                    }
                    let menu = match &url {
                        Some(url) => menu
                            .menu(
                                crate::tr!("pull_requests.conversation.copy_comment_link")
                                    .into_owned(),
                                Box::new(CopyText(url.clone())),
                            )
                            .menu(
                                crate::tr!("pull_requests.open_on_host", host_name = &host_name)
                                    .into_owned(),
                                Box::new(OpenUrl(url.clone())),
                            ),
                        None => menu,
                    };
                    menu.menu(
                        crate::tr!("pull_requests.conversation.copy_text").into_owned(),
                        Box::new(CopyText(body.clone())),
                    )
                })
            })
            .into_any_element()
    }

    /// The comment's reactions with this client's unconfirmed ones applied, in GitHub's order.
    fn shown_reactions(&self, comment: &PullRequestComment) -> Vec<PullRequestReaction> {
        let mut shown = comment.reactions.clone();
        if let Some(writes) = self.writes() {
            for ((subject, content), reacted) in &writes.reactions {
                if *subject != comment.id {
                    continue;
                }
                match shown
                    .iter_mut()
                    .find(|reaction| reaction.content == *content)
                {
                    Some(reaction) if reaction.viewer_reacted != *reacted => {
                        reaction.viewer_reacted = *reacted;
                        reaction.count = if *reacted {
                            reaction.count + 1
                        } else {
                            reaction.count.saturating_sub(1)
                        };
                    }
                    Some(_) => {}
                    None if *reacted => shown.push(PullRequestReaction {
                        content: *content,
                        count: 1,
                        viewer_reacted: true,
                    }),
                    None => {}
                }
            }
        }
        shown.retain(|reaction| reaction.count > 0);
        shown.sort_by_key(|reaction| {
            PullRequestReactionContent::ALL
                .iter()
                .position(|content| *content == reaction.content)
        });
        shown
    }

    /// The eight reactions behind an add chip. `Sticker` stands in for a smile-plus glyph,
    /// which gpui-kit's icon set lacks.
    fn reaction_popover(
        &self,
        comment: &PullRequestComment,
        head: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.compact(cx);
        let id = comment.id.clone();
        let sheet = Sheet::Reactions(id.clone());
        let own: Vec<_> = self
            .shown_reactions(comment)
            .into_iter()
            .filter(|reaction| reaction.viewer_reacted)
            .map(|reaction| reaction.content)
            .collect();
        let view = cx.entity();
        let open_id = id.clone();
        // In the head row a pointer that can hover finds it on hover; a keyboard on focus.
        let reveal = head && !compact && !crate::window_seam::is_mobile(cx);
        let chip = reaction_chip(
            SharedString::from(format!("pr-reaction-add-{id}-{head}")),
            crate::tr!("pull_requests.reactions.add").into_owned(),
            false,
            cx,
        )
        .icon(IconName::Sticker)
        .tooltip(crate::tr!("pull_requests.reactions.add"))
        .when(reveal, |chip| {
            // `focus`, not `focus_visible`: the Button paints its ring through `focus_visible`.
            chip.opacity(0.)
                .group_hover(
                    SharedString::from(format!("pr-comment-head-{id}")),
                    |chip| chip.opacity(1.),
                )
                .focus(|chip| chip.opacity(1.))
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            this.open_sheet(Some(Sheet::Reactions(open_id.clone())), cx)
        }));
        let popover = Popover::new(SharedString::from(format!("pr-reactions-{id}-{head}")))
            .open(self.sheet_open(&sheet))
            .on_open_change({
                let view = view.clone();
                move |open, _, cx| {
                    if !*open {
                        view.update(cx, |view, cx| view.open_sheet(None, cx));
                    }
                }
            })
            .trigger_with(move |_, _, _, _| chip.into_any_element());
        let popover = if compact {
            popover.bottom_sheet(crate::tr!("pull_requests.reactions.add").into_owned())
        } else {
            popover
        };
        popover
            .content(move |_, _, cx| {
                h_flex()
                    .p_1()
                    .gap_1()
                    .children(PullRequestReactionContent::ALL.into_iter().map(|content| {
                        let reacted = own.contains(&content);
                        let (view, id) = (view.clone(), id.clone());
                        Button::new(SharedString::from(format!("pr-reaction-{id}-{content:?}")))
                            .ghost()
                            .chip()
                            .touch(compact)
                            .selected(reacted)
                            .aria_label(crate::tr!(reaction_name(content)).into_owned())
                            .aria_toggled(if reacted {
                                gpui::Toggled::True
                            } else {
                                gpui::Toggled::False
                            })
                            .text_token(&cx.theme().tokens.typography.md)
                            .child(reaction_emoji(content))
                            .on_click(move |_, window, cx| {
                                view.update(cx, |view, cx| {
                                    view.react(id.clone(), content, !reacted, window, cx)
                                })
                            })
                    }))
            })
            .into_any_element()
    }

    fn reactions(
        &self,
        comment: &PullRequestComment,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let shown = self.shown_reactions(comment);
        if shown.is_empty() {
            return None;
        }
        let interactive = self.writable(cx) && self.can_react(comment);
        let add = interactive.then(|| self.reaction_popover(comment, false, cx));
        Some(
            h_flex()
                .flex_wrap()
                .gap_1()
                .pt_1()
                .children(shown.into_iter().map(|reaction| {
                    let own = reaction.viewer_reacted;
                    let label = crate::tr!(
                        "pull_requests.reactions.chip_label",
                        name = crate::tr!(reaction_name(reaction.content)).into_owned(),
                        count = reaction.count.to_string()
                    )
                    .into_owned();
                    let chip = reaction_chip(
                        SharedString::from(format!(
                            "pr-reaction-chip-{}-{:?}",
                            comment.id, reaction.content
                        )),
                        label,
                        own,
                        cx,
                    )
                    .aria_toggled(if own {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .child(reaction_emoji(reaction.content))
                    .child(reaction.count.to_string());
                    if interactive {
                        let (id, content) = (comment.id.clone(), reaction.content);
                        chip.on_click(cx.listener(move |this, _, window, cx| {
                            this.react(id.clone(), content, !own, window, cx)
                        }))
                        .into_any_element()
                    } else {
                        chip.into_any_element()
                    }
                }))
                .children(add)
                .into_any_element(),
        )
    }

    fn body(
        &self,
        comment: &PullRequestComment,
        clamp: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let body = visible_body(&comment.body)?;
        let state = self.markdown(&comment.id, &body, cx);
        let expanded = self
            .page()
            .is_some_and(|page| page.conversation_view.expanded.contains(&comment.id));
        // A long body is clipped until the reader asks for all of it.
        let long = clamp && body.lines().count() > 16;
        let id = comment.id.clone();
        Some(
            v_flex()
                .min_w_0()
                .text_body(self.compact(cx), cx)
                .child(
                    div()
                        .min_w_0()
                        .when(long && !expanded, |body| {
                            body.max_h(px(320.)).overflow_hidden()
                        })
                        .child(
                            MarkdownView::new(&state)
                                .compact_headings(true)
                                .selectable(true),
                        ),
                )
                .when(long, |body| {
                    body.child(
                        div().pt_1().child(
                            Button::new(SharedString::from(format!("pr-comment-more-{id}")))
                                .ghost()
                                .xsmall()
                                .label(if expanded {
                                    crate::tr!("pull_requests.conversation.show_less")
                                } else {
                                    crate::tr!("pull_requests.conversation.show_full")
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_expanded(&id, cx);
                                })),
                        ),
                    )
                })
                .into_any_element(),
        )
    }

    fn toggle_expanded(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(page) = self.page_mut() {
            let view = &mut page.conversation_view;
            if !view.expanded.remove(id) {
                view.expanded.insert(id.to_owned());
            }
            view.list.remeasure();
        }
        cx.notify();
    }

    fn comment_card(
        &mut self,
        comment: &PullRequestComment,
        description: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.compact(cx);
        let muted = cx.theme().muted_foreground;
        let verdict = comment.review_state.map(|state| {
            let (icon, color, label) = match state {
                PullRequestReviewState::Approved => (
                    IconName::BadgeCheck,
                    cx.theme().success,
                    "pull_requests.conversation.approved",
                ),
                PullRequestReviewState::ChangesRequested => (
                    IconName::MessageSquareWarning,
                    cx.theme().danger,
                    "pull_requests.conversation.changes_requested",
                ),
                PullRequestReviewState::Dismissed => (
                    IconName::MessageSquare,
                    muted,
                    "pull_requests.conversation.dismissed",
                ),
                PullRequestReviewState::Commented | PullRequestReviewState::Pending => (
                    IconName::MessageSquare,
                    muted,
                    "pull_requests.conversation.reviewed",
                ),
            };
            h_flex()
                .gap_1()
                .items_center()
                .text_color(color)
                .child(Icon::new(icon))
                .child(crate::tr!(label))
                .into_any_element()
        });
        let action = verdict.or_else(|| {
            Some(
                div()
                    .text_color(muted)
                    .child(crate::tr!(if description {
                        "pull_requests.conversation.opened"
                    } else {
                        "pull_requests.conversation.commented"
                    }))
                    .into_any_element(),
            )
        });
        let head = self.comment_head(
            comment,
            action,
            if compact { 24. } else { 20. },
            Place {
                description,
                thread: None,
            },
            cx,
        );
        let slot = if description {
            Slot::Description
        } else {
            Slot::Edit(comment.id.clone())
        };
        let body = self
            .editing_body(&slot, cx)
            .or_else(|| self.body(comment, true, cx));
        let empty_description = description && body.is_none();
        v_flex()
            .w_full()
            .min_w_0()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(head)
            .children(body)
            .when(empty_description, |card| {
                card.child(
                    div()
                        .text_body(compact, cx)
                        .text_color(muted)
                        .child(crate::tr!("pull_requests.conversation.no_description")),
                )
            })
            .children(self.reactions(comment, cx))
            .into_any_element()
    }

    fn read_replies(&mut self, thread: &PullRequestReviewThread, cx: &mut Context<Self>) {
        let id = thread.id.clone();
        let after = self
            .page()
            .and_then(|page| page.conversation_view.replies.get(&id))
            .and_then(|replies| replies.after.clone())
            .or_else(|| thread.replies_after.clone());
        let Some(after) = after else { return };
        let Some(page) = self.page_mut() else { return };
        let replies = page
            .conversation_view
            .replies
            .entry(id.clone())
            .or_insert_with(|| Replies {
                after: Some(after.clone()),
                ..Default::default()
            });
        replies.loading = true;
        replies.error = None;
        let key = id.clone();
        self.read(
            PullRequestRead::ThreadReplies {
                thread_id: id,
                after,
            },
            cx,
            move |page, result| {
                let replies = page.conversation_view.replies.entry(key).or_default();
                replies.loading = false;
                match result {
                    Ok((PullRequestReadResponse::ThreadReplies(more), _)) => {
                        replies.comments.extend(more.comments);
                        replies.after = more.after;
                        page.conversation_view.list.remeasure();
                    }
                    Ok(_) => {}
                    Err(error) => {
                        replies.error = Some(reason(&error));
                        page.conversation_view.list.remeasure();
                    }
                }
            },
        );
    }

    /// A review thread: the head line and first comment inline on the diff, the whole thread
    /// in the conversation.
    pub(super) fn thread_card(
        &self,
        thread: &PullRequestReviewThread,
        inline: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let compact = self.compact(cx);
        let expanded = self
            .page()
            .is_some_and(|page| page.conversation_view.expanded.contains(&thread.id));
        let collapsed = thread.resolved && !expanded;
        let lines = thread.anchor.as_ref().map(|anchor| {
            let lines = if anchor.start_line == anchor.end_line {
                crate::tr!(
                    "pull_requests.conversation.line",
                    line = anchor.end_line.to_string()
                )
            } else {
                crate::tr!(
                    "pull_requests.conversation.lines",
                    start = anchor.start_line.to_string(),
                    end = anchor.end_line.to_string()
                )
            }
            .into_owned();
            if anchor.side == ReviewSide::Old {
                format!(
                    "{lines} {}",
                    crate::tr!("pull_requests.conversation.base_side")
                )
            } else {
                lines
            }
        });
        let rail = if thread.resolved || thread.outdated {
            muted
        } else {
            cx.theme().primary
        };
        let count = thread.total_comments.max(thread.comments.len() as u64);
        let id = thread.id.clone();
        let thread_for_files = thread.clone();
        let current = thread.anchor.is_some() && !thread.outdated;
        let chip = |label: &'static str, cx: &App| {
            material::semantic_chip(
                crate::tr!(label).into_owned(),
                cx.theme().secondary,
                muted,
                cx,
            )
        };
        let head = material::card_header_row(cx)
            .text_secondary(compact, cx)
            .font_family(cx.theme().font_family.clone())
            .when(!inline, |head| {
                head.child(Icon::new(IconName::File)).child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_meta_mono(compact, cx)
                        .text_color(cx.theme().foreground)
                        .child(crate::widgets::fade_text(match thread.anchor.as_ref() {
                            Some(anchor) if anchor.start_line != anchor.end_line => {
                                format!("{}:{}–{}", thread.path, anchor.start_line, anchor.end_line)
                            }
                            Some(anchor) => format!("{}:{}", thread.path, anchor.end_line),
                            None => thread.path.clone(),
                        })),
                )
            })
            .when(inline, |head| head.children(lines.clone()))
            .when(thread.outdated, |head| {
                head.child(chip("pull_requests.conversation.outdated", cx))
            })
            .when(thread.resolved, |head| {
                head.child(chip("pull_requests.conversation.resolved", cx))
            })
            .child(
                crate::tr!(
                    if count == 1 {
                        "pull_requests.conversation.comment_count_one"
                    } else {
                        "pull_requests.conversation.comment_count"
                    },
                    count = count.to_string()
                )
                .into_owned(),
            )
            .child(div().flex_1())
            .when(inline, |head| {
                let id = id.clone();
                head.child(
                    Button::new(SharedString::from(format!("pr-thread-show-{}", thread.id)))
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.files.show_in_conversation"))
                        .on_click(cx.listener(move |this, _, _, cx| this.show_thread(&id, cx))),
                )
            })
            .when(!inline && current, |head| {
                head.child(
                    Button::new(SharedString::from(format!("pr-thread-files-{}", thread.id)))
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.conversation.view_in_files"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.show_in_files(&thread_for_files, cx)
                        })),
                )
            });
        let head_id = SharedString::from(format!("pr-thread-head-{}-{inline}", thread.id));
        // A resolved thread's head is its disclosure.
        let head = if thread.resolved {
            let id = id.clone();
            material::accessible_clickable(
                head,
                head_id,
                gpui::Role::Button,
                crate::tr!("pull_requests.conversation.resolved").into_owned(),
                cx,
            )
            .aria_expanded(!collapsed)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(&id, cx)))
            .into_any_element()
        } else {
            head.id(head_id).into_any_element()
        };
        // The header row carries the card inset itself, so the card's own inset is moved onto
        // the rows below it.
        let mut card = material::card(cx)
            .min_w_full()
            .my_1()
            .px_0()
            .pt_0()
            .when(collapsed, |card| card.pb_0())
            .relative()
            .font_family(cx.theme().font_family.clone())
            .child(
                // The thread's accent rail; no token names an accent edge's width or inset.
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(6.))
                    .bottom(px(6.))
                    .w(px(2.))
                    .rounded_full()
                    .bg(rail),
            )
            .child(head);
        if collapsed {
            if !inline && self.can_resolve(thread) && self.writable(cx) {
                let place = thread.path.clone();
                card = card.child(
                    h_flex()
                        .px(px(material::CARD_INSET))
                        .pb_1()
                        .justify_end()
                        .child(self.resolve_button(thread, &place, cx)),
                );
            }
            return card.into_any_element();
        }
        if !inline && let Some(hunk) = &thread.diff_hunk {
            card = card.child(
                v_flex()
                    .mx(px(material::CARD_INSET))
                    .text_code(cx)
                    .children(hunk.lines().map(|line| {
                        let bg = match line.chars().next() {
                            Some('+') => Some(cx.theme().success.opacity(0.13)),
                            Some('-') => Some(cx.theme().danger.opacity(0.12)),
                            _ => None,
                        };
                        div()
                            .px_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .when_some(bg, |row, bg| row.bg(bg))
                            .child(line.to_owned())
                    })),
            );
        }
        let replies = self
            .page()
            .and_then(|page| page.conversation_view.replies.get(&thread.id));
        let mut comments: Vec<PullRequestComment> = thread.comments.clone();
        if !inline && let Some(replies) = replies {
            comments.extend(replies.comments.iter().cloned());
        }
        let shown = if inline { 1 } else { comments.len() };
        for comment in comments.into_iter().take(shown) {
            let head = self.comment_head(
                &comment,
                None,
                16.,
                Place {
                    description: false,
                    thread: Some(thread.id.clone()),
                },
                cx,
            );
            let body = (!inline)
                .then(|| self.editing_body(&Slot::Edit(comment.id.clone()), cx))
                .flatten()
                .or_else(|| self.body(&comment, inline, cx));
            card = card.child(
                v_flex()
                    .px(px(material::CARD_INSET))
                    .gap_1()
                    .child(head)
                    .children(body)
                    .children(self.reactions(&comment, cx)),
            );
        }
        if inline {
            return card.into_any_element();
        }
        let loading = replies.is_some_and(|replies| replies.loading);
        let after = replies.map_or(thread.replies_after.clone(), |replies| {
            replies.after.clone()
        });
        let read = thread.comments.len() as u64
            + replies.map_or(0, |replies| replies.comments.len() as u64);
        if after.is_some() {
            let remaining = thread.total_comments.saturating_sub(read);
            let thread = thread.clone();
            card = card.child(
                div().px(px(material::CARD_INSET)).child(
                    Button::new(SharedString::from(format!("pr-thread-more-{}", thread.id)))
                        .ghost()
                        .xsmall()
                        .disabled(loading)
                        .label(if loading {
                            crate::tr!("pull_requests.conversation.loading")
                        } else if remaining == 1 {
                            crate::tr!("pull_requests.conversation.more_replies_one")
                        } else if remaining > 1 {
                            crate::tr!(
                                "pull_requests.conversation.more_replies",
                                count = remaining.to_string()
                            )
                        } else {
                            crate::tr!("pull_requests.conversation.more_replies_unknown")
                        })
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.read_replies(&thread, cx)),
                        ),
                ),
            );
            if let Some(error) = replies.and_then(|replies| replies.error.clone()) {
                card = card.child(
                    div()
                        .px(px(material::CARD_INSET))
                        .text_secondary(compact, cx)
                        .text_color(cx.theme().warning)
                        .child(
                            crate::tr!("pull_requests.conversation.replies_failed", reason = error)
                                .into_owned(),
                        ),
                );
            }
        } else if read < thread.total_comments {
            card = card.child(
                div()
                    .px(px(material::CARD_INSET))
                    .text_secondary(compact, cx)
                    .child(crate::tr!("pull_requests.conversation.replies_capped")),
            );
        }
        card.children(self.thread_footer(thread, cx))
            .into_any_element()
    }

    /// What the host offers at all, as the conversation carries it.
    pub(super) fn capabilities(&self) -> PullRequestCapabilities {
        self.page()
            .and_then(|page| page.conversation.data.as_ref())
            .map_or(PullRequestCapabilities::ALL, |conversation| {
                conversation.capabilities
            })
    }

    fn can_reply(&self, thread: &PullRequestReviewThread) -> bool {
        self.capabilities().reply && thread.viewer_can_reply
    }

    fn can_resolve(&self, thread: &PullRequestReviewThread) -> bool {
        self.capabilities().resolve && thread.viewer_can_resolve
    }

    fn can_react(&self, comment: &PullRequestComment) -> bool {
        self.capabilities().reactions && comment.viewer_can_react
    }

    /// Reply and Resolve under a whole thread, as the host says this account may.
    fn thread_footer(
        &self,
        thread: &PullRequestReviewThread,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.writable(cx) || !(self.can_reply(thread) || self.can_resolve(thread)) {
            return None;
        }
        let compact = self.compact(cx);
        let slot = Slot::Reply(thread.id.clone());
        let place = thread.anchor.as_ref().map_or_else(
            || thread.path.clone(),
            |anchor| format!("{}:{}", thread.path, anchor.end_line),
        );
        let line = thread
            .anchor
            .as_ref()
            .map_or_else(String::new, |anchor| anchor.end_line.to_string());
        let spec = EditorSpec {
            context: Some(
                crate::tr!(
                    "pull_requests.compose.replying_to",
                    path = thread.path.clone(),
                    line = line.clone()
                )
                .into_owned(),
            ),
            submit: crate::tr!("pull_requests.compose.reply").into(),
            aria: crate::tr!("pull_requests.compose.reply_placeholder").into(),
            cancel: true,
        };
        let reply_slot = slot.clone();
        let reply: Option<AnyElement> = self.can_reply(thread).then(|| {
            if compact {
                self.editor_sheet(
                    slot.clone(),
                    crate::tr!("pull_requests.compose.reply_row").into(),
                    crate::tr!(
                        "pull_requests.compose.reply_sheet",
                        path = thread.path.clone(),
                        line = line.clone()
                    )
                    .into(),
                    spec.clone(),
                    move |this, window, cx| {
                        this.open_editor(
                            reply_slot.clone(),
                            crate::tr!("pull_requests.compose.reply_placeholder").into(),
                            String::new(),
                            None,
                            window,
                            cx,
                        )
                    },
                    cx,
                )
            } else {
                match self.editor_element(&cx.entity(), &slot, spec.clone(), cx) {
                    Some(editor) => editor,
                    None => div()
                        .id(SharedString::from(format!("pr-reply-row-{}", thread.id)))
                        // A field placeholder that becomes an editor on click, not a button.
                        .h(px(material::input_height(false)))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded(material::radius_input(cx))
                        .border_1()
                        .border_color(cx.theme().input)
                        .bg(cx.theme().background)
                        .text_body(false, cx)
                        .text_color(cx.theme().muted_foreground)
                        .cursor_text()
                        .child(crate::tr!("pull_requests.compose.reply_row"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_editor(
                                reply_slot.clone(),
                                crate::tr!("pull_requests.compose.reply_placeholder").into(),
                                String::new(),
                                None,
                                window,
                                cx,
                            );
                            if let Some(page) = this.page() {
                                page.conversation_view.list.remeasure();
                            }
                        }))
                        .into_any_element(),
                }
            }
        });
        let resolve = self
            .can_resolve(thread)
            .then(|| self.resolve_button(thread, &place, cx));
        Some(
            h_flex()
                .px(px(material::CARD_INSET))
                .gap_2()
                .items_center()
                .children(reply.map(|reply| div().flex_1().min_w_0().child(reply)))
                .when(!self.can_reply(thread), |footer| {
                    footer.child(div().flex_1())
                })
                .children(resolve)
                .into_any_element(),
        )
    }

    fn resolve_button(
        &self,
        thread: &PullRequestReviewThread,
        place: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let resolved = thread.resolved;
        let id = thread.id.clone();
        let busy = self.busy(&format!("resolve {id}"));
        Button::new(SharedString::from(format!("pr-thread-resolve-{id}")))
            .ghost()
            .xsmall()
            .loading(busy)
            .icon(if resolved {
                IconName::RotateCcw
            } else {
                IconName::Check
            })
            .label(crate::tr!(if resolved {
                "pull_requests.compose.unresolve"
            } else {
                "pull_requests.compose.resolve"
            }))
            .tooltip(
                crate::tr!(
                    "pull_requests.compose.resolve_label",
                    path = thread.path.clone(),
                    line = place.rsplit(':').next().unwrap_or_default().to_owned()
                )
                .into_owned(),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.resolve(id.clone(), !resolved, window, cx)
            }))
            .into_any_element()
    }

    /// The editor standing in for a body being edited.
    fn editing_body(&self, slot: &Slot, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.editor_element(
            &cx.entity(),
            slot,
            EditorSpec {
                context: None,
                submit: crate::tr!("pull_requests.compose.save").into(),
                aria: crate::tr!("pull_requests.compose.edit").into(),
                cancel: true,
            },
            cx,
        )
    }

    /// The conversation's last row: the comment composer.
    fn composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let number = self
            .current
            .as_ref()
            .map(|(_, key)| key.number.to_string())
            .unwrap_or_default();
        let spec = EditorSpec {
            context: None,
            submit: crate::tr!("pull_requests.compose.comment").into(),
            aria: crate::tr!(
                "pull_requests.compose.comment_label",
                number = number.clone()
            )
            .into(),
            cancel: false,
        };
        let editor = if self.compact(cx) {
            self.editor_sheet(
                Slot::Composer,
                crate::tr!("pull_requests.compose.add_comment_row").into(),
                crate::tr!("pull_requests.compose.comment_label", number = number).into(),
                spec,
                |this, window, cx| {
                    this.open_editor(
                        Slot::Composer,
                        crate::tr!("pull_requests.compose.comment_placeholder").into(),
                        String::new(),
                        None,
                        window,
                        cx,
                    )
                },
                cx,
            )
        } else if self.read_only(cx) {
            div()
                .text_secondary(self.compact(cx), cx)
                .child(crate::tr!("pull_requests.compose.read_only"))
                .into_any_element()
        } else {
            self.editor_element(&cx.entity(), &Slot::Composer, spec, cx)
                .unwrap_or_else(|| div().into_any_element())
        };
        v_flex()
            .py_3()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(editor)
            .into_any_element()
    }

    fn render_item(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let items = self.items(cx);
        let Some(item) = items.get(index).cloned() else {
            return div().into_any_element();
        };
        let Some(conversation) = self.page().and_then(|page| page.conversation.data.clone()) else {
            return div().into_any_element();
        };
        let row = match item {
            Item::Notice => {
                let url = self.url(cx);
                let host_name = self.host_name(cx);
                h_flex()
                    .my_2()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(crate::diff::list::render_notice(
                        crate::tr!("pull_requests.conversation.capped_unknown").into_owned(),
                        cx,
                    )))
                    .children(url.map(|url| {
                        Button::new("pr-capped-open")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!(
                                "pull_requests.open_on_host",
                                host_name = &host_name
                            ))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    }))
                    .into_any_element()
            }
            Item::Meta => match self.meta_block(cx) {
                Some(block) => block,
                None => div().into_any_element(),
            },
            Item::PendingReview => match self.pending_review_line(cx) {
                Some(line) => line,
                None => div().into_any_element(),
            },
            Item::Composer => self.composer(cx),
            Item::Description => self.comment_card(&conversation.description, true, cx),
            Item::Comment(index) => match conversation.comments.get(index) {
                Some(comment) => self.comment_card(comment, false, cx),
                None => div().into_any_element(),
            },
            Item::Thread(index) => match conversation.threads.get(index) {
                Some(thread) => div()
                    .py_1()
                    .child(self.thread_card(thread, false, cx))
                    .into_any_element(),
                None => div().into_any_element(),
            },
            Item::Event(state, at) => {
                let ago = ago_rfc3339(&at).unwrap_or_default();
                let (icon, label) = match state {
                    PullRequestState::Merged => (
                        IconName::GitMerge,
                        crate::tr!("pull_requests.conversation.merged", ago = ago),
                    ),
                    _ => (
                        IconName::GitPullRequestClosed,
                        crate::tr!("pull_requests.conversation.closed", ago = ago),
                    ),
                };
                h_flex()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .text_secondary(self.compact(cx), cx)
                    .child(Icon::new(icon))
                    .child(label)
                    .into_any_element()
            }
        };
        let compact = self.compact(cx);
        div()
            .px(if compact {
                px(material::COMPACT_PAGE_INSET)
            } else {
                cx.theme().tokens.spacing.sm
            })
            .child(row)
            .into_any_element()
    }

    pub(super) fn render_conversation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The desktop composer is always open at the end of the list.
        if !self.compact(cx)
            && !self.read_only(cx)
            && self
                .page()
                .is_some_and(|page| page.conversation.data.is_some())
            && self
                .writes()
                .is_some_and(|writes| !writes.editors.contains_key(&Slot::Composer))
        {
            self.open_composer(window, cx);
        }
        let Some(page) = self.page() else {
            return div().into_any_element();
        };
        let Some(conversation) = page.conversation.data.as_ref() else {
            return match &page.conversation.error {
                Some(error) => {
                    let error = error.clone();
                    self.failure(
                        &error,
                        crate::tr!("pull_requests.conversation.load_failed").into_owned(),
                        cx,
                    )
                }
                None => material::loading_skeleton(cx),
            };
        };
        let empty = visible_body(&conversation.description.body).is_none()
            && conversation.comments.is_empty()
            && conversation.threads.is_empty();
        if empty {
            // The first comment can still be written under the empty state.
            let compact = self.compact(cx);
            return v_flex()
                .id("pr-conversation-empty")
                .size_full()
                .overflow_y_scroll()
                .children(self.meta_block(cx).map(|block| {
                    div()
                        .px(if compact {
                            px(material::COMPACT_PAGE_INSET)
                        } else {
                            cx.theme().tokens.spacing.sm
                        })
                        .child(block)
                }))
                .child(
                    material::empty_state(
                        Icon::new(IconName::MessageSquare),
                        crate::tr!("pull_requests.conversation.empty_title").into_owned(),
                        crate::tr!("pull_requests.conversation.empty_desc").into_owned(),
                        cx,
                    )
                    .flex_1(),
                )
                .child(
                    div()
                        .px(if compact {
                            px(material::COMPACT_PAGE_INSET)
                        } else {
                            cx.theme().tokens.spacing.sm
                        })
                        .child(self.composer(cx)),
                )
                .into_any_element();
        }
        let count = self.items(cx).len();
        let Some(page) = self.page() else {
            return div().into_any_element();
        };
        let state = page.conversation_view.list.clone();
        if state.item_count() != count {
            state.reset(count);
        }
        let view = cx.entity();
        div()
            .id("pr-conversation")
            .flex_1()
            .min_h_0()
            .size_full()
            .child(
                list(state, move |index, _, cx| {
                    view.update(cx, |view, cx| view.render_item(index, cx))
                })
                .size_full(),
            )
            .into_any_element()
    }
}

/// Where a comment sits, which decides what its menu offers.
struct Place {
    description: bool,
    thread: Option<String>,
}

impl PullRequestView {
    fn open_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = window.focused(cx);
        self.open_editor(
            Slot::Composer,
            crate::tr!("pull_requests.compose.comment_placeholder").into(),
            String::new(),
            None,
            window,
            cx,
        );
        // Opening the standing composer does not take the focus from where the reader is.
        match focus {
            Some(focus) => window.focus(&focus, cx),
            None => window.blur(cx),
        }
    }
}

/// What stands in for host media until it can be drawn.
fn media_stand_in(
    state: MediaState,
    url: &str,
    title: &str,
    host_name: &str,
    cx: &App,
) -> Option<AnyElement> {
    // Alt text may run over lines; these rows truncate on one.
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title.as_str();
    let muted = cx.theme().muted_foreground;
    let open = |label: SharedString, url: String| {
        Button::new(SharedString::from(format!("pr-media-open-{url}")))
            .ghost()
            .xsmall()
            .icon(IconName::ExternalLink)
            .label(label)
            .on_click(move |_, _, cx| cx.open_url(&url))
    };
    let unavailable = |text: String, cx: &App| {
        material::card(cx)
            .flex_row()
            .w_full()
            .items_center()
            .text_secondary(false, cx)
            .child(Icon::new(IconName::ImageOff))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(if title.trim().is_empty() {
                        text
                    } else {
                        format!("{text} · {title}")
                    })),
            )
            .child(open(
                crate::tr!("pull_requests.open_on_host", host_name = host_name)
                    .into_owned()
                    .into(),
                url.to_owned(),
            ))
            .into_any_element()
    };
    match state {
        MediaState::Ready => None,
        MediaState::Loading => Some(
            // Holds an image's place while it loads; its size is unknown until then.
            div()
                .w_full()
                .h(px(180.))
                .max_h(px(240.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(material::radius_card(cx))
                .bg(cx.theme().muted)
                .child(Icon::new(IconName::Image).size(px(20.)).text_color(muted))
                .into_any_element(),
        ),
        MediaState::Unavailable => Some(unavailable(
            crate::tr!("pull_requests.media.unavailable").into_owned(),
            cx,
        )),
        MediaState::TooLarge => Some(unavailable(
            crate::tr!(
                "pull_requests.media.too_large",
                size =
                    crate::thread_export::format_size(tcode_protocol::MAX_PULL_REQUEST_MEDIA_BYTES)
            )
            .into_owned(),
            cx,
        )),
        MediaState::External(mime) => {
            let audio = mime.starts_with("audio/");
            let name = if title.trim().is_empty() {
                url.rsplit('/')
                    .next()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        crate::tr!(if audio {
                            "pull_requests.media.audio"
                        } else {
                            "pull_requests.media.video"
                        })
                        .into_owned()
                    })
            } else {
                title.to_owned()
            };
            Some(
                material::card_secondary(cx)
                    .flex_row()
                    .w_full()
                    .items_center()
                    .text_body(false, cx)
                    .child(
                        Icon::new(if audio {
                            IconName::Music
                        } else {
                            IconName::Film
                        })
                        .text_color(muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(name)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("pr-media-external-{url}")))
                            .outline()
                            .xsmall()
                            .icon(IconName::ExternalLink)
                            .label(crate::tr!("pull_requests.media.open_in_browser"))
                            .on_click({
                                let url = url.to_owned();
                                move |_, _, cx| cx.open_url(&url)
                            }),
                    )
                    .into_any_element(),
            )
        }
    }
}

use super::super::*;

impl Composer {
    pub(in super::super) fn render_checkout_row(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let checkout = self.workspace_store.read(cx).composer_state().checkout?;
        let branch = checkout.branch;
        let branches: Rc<[String]> = checkout.branches.into();
        let checkout_blocked = checkout.checkout_blocked;
        let is_draft = checkout.is_draft;
        let worktree_base = checkout.worktree_base;
        let worktree = checkout.worktree;
        let muted = cx.theme().muted_foreground;
        // In worktree draft mode the right-hand picker chooses the *base* branch
        // (its current value is the chosen base, defaulting to the live branch).
        let picker_current = worktree_base.clone().unwrap_or_else(|| branch.clone());
        let worktree_mode = worktree_base.is_some();

        let right: AnyElement = if checkout_blocked {
            Button::new("branch-picker")
                .ghost()
                .outline()
                .compact()
                .disabled(true)
                .tooltip(crate::tr!("composer.wait_turn"))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_color(muted)
                        .child(Icon::empty().path("icons/git-branch.svg"))
                        .child(picker_current.clone()),
                )
                .into_any_element()
        } else {
            let store_open = self.workspace_store.clone();
            let store_content = self.workspace_store.clone();
            let current: Rc<str> = picker_current.as_str().into();
            let compact = self.compact;
            let trigger = Button::new("branch-picker")
                .ghost()
                .outline()
                .compact()
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_color(muted)
                        .child(Icon::empty().path("icons/git-branch.svg"))
                        .child(picker_current.clone())
                        .child(Icon::new(IconName::ChevronDown)),
                );
            crate::material::overlay_popover("branch-popover", cx)
                .anchor(Anchor::BottomRight)
                .trigger(trigger)
                .on_open_change(move |open, _window, cx| {
                    if *open {
                        store_open.update(cx, |store, _cx| store.load_branches());
                    }
                })
                .content(move |_state, _window, cx| {
                    let spacing = &cx.theme().tokens.spacing;
                    let mut col = v_flex()
                        .w(px(220.))
                        .max_h(px(280.))
                        .pt_1()
                        .px_1()
                        // The last branch row carries its gap.
                        .pb(if branches.is_empty() {
                            spacing.xs
                        } else {
                            spacing.xxs
                        })
                        .gap_0p5();
                    if worktree_mode {
                        col = col.child(
                            div()
                                .flex_none()
                                .px_2()
                                .py_1()
                                .text_caption(cx)
                                .child(crate::tr!("composer.worktree_base")),
                        );
                    }
                    if branches.is_empty() {
                        return col
                            .child(
                                crate::material::empty_row(crate::tr!("composer.loading"), cx)
                                    .flex_none(),
                            )
                            .into_any_element();
                    }
                    let branches = branches.clone();
                    let current = current.clone();
                    let store = store_content.clone();
                    let popover = cx.entity();
                    col.child(
                        crate::scroll::VirtualList::uniform(
                            "branch-list",
                            branches.len(),
                            move |range, _, cx| {
                                range
                                    .map(|index| {
                                        let store = store.clone();
                                        let popover = popover.clone();
                                        branch_row(
                                            index,
                                            &branches[index],
                                            branches[index] == *current,
                                            compact,
                                            move |branch_name, window, cx| {
                                                store.update(cx, |store, _cx| {
                                                    if worktree_mode {
                                                        store.set_draft_workspace(
                                                            WorkspaceMode::NewWorktree {
                                                                base: branch_name,
                                                            },
                                                        );
                                                    } else {
                                                        store.checkout_branch(branch_name);
                                                    }
                                                });
                                                popover.update(cx, |st, cx| st.dismiss(window, cx));
                                            },
                                            cx,
                                        )
                                    })
                                    .collect()
                            },
                        )
                        .w_full()
                        .min_h_0(),
                    )
                    .into_any_element()
                })
                .into_any_element()
        };

        let left =
            self.render_workspace_chip(is_draft, worktree_mode, worktree.is_some(), &branch, cx);

        Some(
            h_flex()
                .w_full()
                .px_2()
                .items_center()
                .justify_between()
                .text_secondary(self.compact, cx)
                .child(left)
                .child(right)
                .into_any_element(),
        )
    }

    /// The left-hand workspace chip: a draft can pick current checkout vs a new
    /// dedicated worktree; a started session shows its locked workspace.
    pub(in super::super) fn render_workspace_chip(
        &self,
        is_draft: bool,
        worktree_mode: bool,
        has_worktree: bool,
        base_default: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let label = if worktree_mode || has_worktree {
            crate::tr!("composer.new_worktree")
        } else {
            crate::tr!("composer.local_checkout")
        };

        if !is_draft {
            return h_flex()
                .gap_1p5()
                .items_center()
                .text_color(muted)
                .child(Icon::empty().path("icons/folder-closed.svg"))
                .child(label)
                .into_any_element();
        }

        let store_content = self.workspace_store.clone();
        let base_default = base_default.to_string();
        let compact = self.compact;
        let trigger = Button::new("workspace-picker")
            .ghost()
            .outline()
            .compact()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_color(muted)
                    .child(Icon::empty().path("icons/folder-closed.svg"))
                    .child(label)
                    .child(Icon::new(IconName::ChevronDown)),
            );
        crate::material::overlay_popover("workspace-popover", cx)
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let popover = cx.entity();
                let store_local = store_content.clone();
                let store_worktree = store_content.clone();
                let pop_local = popover.clone();
                let pop_worktree = popover.clone();
                let base = base_default.clone();
                let workspace_row = |label: gpui::SharedString,
                                     selected: bool,
                                     cx: &mut Context<PopoverState>|
                 -> gpui::Div {
                    crate::material::menu_item_frame(compact, cx)
                        .w_full()
                        .items_center()
                        .cursor_pointer()
                        .text_body(compact, cx)
                        .hover(|s| s.bg(cx.theme().muted))
                        .child(div().flex_1().min_w_0().child(label))
                        .when(selected, |this| {
                            this.child(Icon::new(IconName::Check).text_color(cx.theme().primary))
                        })
                };
                v_flex()
                    .w(px(200.))
                    .p_1()
                    .gap_0p5()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_caption(cx)
                            .child(crate::tr!("composer.workspace")),
                    )
                    .child(
                        workspace_row(
                            crate::tr!("composer.local_checkout").into_owned().into(),
                            false,
                            cx,
                        )
                        .id("workspace-local")
                        .on_click(move |_, window, cx| {
                            store_local.update(cx, |store, _cx| {
                                store.set_draft_workspace(WorkspaceMode::LocalCheckout);
                            });
                            pop_local.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        workspace_row(
                            crate::tr!("composer.new_worktree").into_owned().into(),
                            false,
                            cx,
                        )
                        .id("workspace-worktree")
                        .on_click(move |_, window, cx| {
                            let base = base.clone();
                            store_worktree.update(cx, |store, _cx| {
                                store.set_draft_workspace(WorkspaceMode::NewWorktree { base });
                            });
                            pop_worktree.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }
}

fn branch_row(
    index: usize,
    name: &str,
    is_current: bool,
    compact: bool,
    on_pick: impl Fn(String, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let branch_name = name.to_string();
    div()
        .pb_0p5()
        .child(
            crate::material::menu_item_frame(compact, cx)
                .id(("branch-row", index))
                .flex_none()
                .w_full()
                .items_center()
                .cursor_pointer()
                .text_body(compact, cx)
                .hover(|s| s.bg(cx.theme().muted))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(name.to_string())),
                )
                .when(is_current, |this| {
                    this.child(Icon::new(IconName::Check).text_color(cx.theme().primary))
                })
                .on_click(move |_, window, cx| on_pick(branch_name.clone(), window, cx)),
        )
        .into_any_element()
}

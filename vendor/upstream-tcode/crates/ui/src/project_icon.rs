//! Shared project artwork and an in-app picker over the attached host's files.
use crate::{
    icon::{Icon, IconName},
    overlay::OverlayExt as _,
    store::{WorkspaceStore, images},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Role, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription,
    UniformListScrollHandle, Window, div, img, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_base::{ElementExt as _, Scrollbar, h_flex, v_flex};
use std::{path::PathBuf, rc::Rc};
use tcode_core::project::Project;
use tcode_protocol::{IconImageEntry, QueryResponse};

const TILE_WIDTH: f32 = 96.;
const TILE_GAP: f32 = 8.;

pub(crate) fn label(project: &Project) -> String {
    if project.quick_chat {
        crate::tr!("sidebar.quick_chat").into_owned()
    } else {
        project.name.clone()
    }
}

pub(crate) fn group_label(project: &Project) -> String {
    if project.quick_chat {
        crate::tr!("sidebar.quick_chats").into_owned()
    } else {
        project.name.clone()
    }
}

pub(crate) fn artwork(project: &Project, size: f32) -> AnyElement {
    if project.quick_chat {
        return Icon::empty()
            .path("icons/message-square.svg")
            .size(px(size))
            .flex_none()
            .into_any_element();
    }
    img(images::project_icon(project, size))
        .size(px(size))
        .flex_none()
        .with_fallback(move || {
            Icon::new(IconName::Folder)
                .size(px(size))
                .into_any_element()
        })
        .into_any_element()
}

pub(crate) fn open(
    store: Entity<WorkspaceStore>,
    project: Project,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|cx| Picker::new(store, project, window, cx));
    picker.update(cx, |picker, cx| {
        picker.browse(picker.project.root.clone(), window, cx)
    });
    window.open_dialog(cx, move |dialog, _, _| {
        let content = picker.clone();
        dialog
            // Enter in a path or filter field must not confirm the dialog.
            .on_ok(|_, _, _| false)
            .title(crate::tr!("project_icon.title"))
            .wide()
            .content(move |el, _, _| el.child(content.clone()))
    });
}

struct Picker {
    store: Entity<WorkspaceStore>,
    project: Project,
    path: Entity<InputState>,
    search: Entity<InputState>,
    parent: Option<PathBuf>,
    entries: Vec<IconImageEntry>,
    selected: Option<PathBuf>,
    loading: bool,
    saving: bool,
    error: Option<String>,
    generation: u64,
    /// Tiles per row derive from the grid's width in the previous frame.
    grid_width: Pixels,
    grid_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl Picker {
    fn new(
        store: Entity<WorkspaceStore>,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = cx.new(|cx| InputState::new(window, cx));
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!("project_icon.search")));
        let subscriptions = vec![
            cx.subscribe_in(&path, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) && !this.saving {
                    this.browse(
                        PathBuf::from(this.path.read(cx).value().as_str()),
                        window,
                        cx,
                    );
                }
            }),
            cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        Self {
            store,
            project,
            path,
            search,
            parent: None,
            entries: Vec::new(),
            selected: None,
            loading: false,
            saving: false,
            error: None,
            generation: 0,
            grid_width: px(0.),
            grid_scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn browse(&mut self, directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        self.selected = None;
        self.entries.clear();
        self.grid_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.path.update(cx, |input, cx| {
            input.set_value(directory.to_string_lossy().to_string(), window, cx)
        });
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
        let task = self
            .store
            .update(cx, |store, cx| store.browse_icon_images(directory, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(QueryResponse::IconImages {
                        directory,
                        parent,
                        entries,
                    }) => {
                        this.path.update(cx, |input, cx| {
                            input.set_value(directory.to_string_lossy().to_string(), window, cx)
                        });
                        this.parent = parent;
                        this.entries = entries;
                    }
                    Err(error) => this.error = Some(error.message),
                    _ => this.error = Some(crate::tr!("project_icon.load_failed").into_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.saving = true;
        self.error = None;
        let task = self.store.update(cx, |store, cx| {
            store.set_project_icon(self.project.id.clone(), path, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(_) => {
                        window.close_dialog(cx);
                    }
                    Err(error) => this.error = Some(error.message),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_tile(&self, entry: &IconImageEntry, cx: &mut Context<Self>) -> AnyElement {
        let path = entry.path.clone();
        let is_dir = entry.is_dir;
        let is_selected = self.selected.as_ref() == Some(&path);
        let content = if is_dir {
            Icon::new(IconName::Folder)
                .size(px(32.))
                .text_color(cx.theme().muted_foreground)
                .into_any_element()
        } else {
            img(images::icon_thumbnail(path.clone()))
                .size(px(64.))
                .with_fallback(|| {
                    Icon::empty()
                        .path("icons/image.svg")
                        .size(px(28.))
                        .into_any_element()
                })
                .into_any_element()
        };
        crate::material::accessible_clickable(
            v_flex(),
            SharedString::from(format!("icon-file-{}", entry.name)),
            Role::Button,
            entry.name.clone(),
            cx,
        )
        .aria_selected(is_selected)
        .debug_selector({
            let name = entry.name.clone();
            move || format!("icon-file-{name}")
        })
        .w(px(TILE_WIDTH))
        .h(px(108.))
        .p_2()
        .gap_1()
        .items_center()
        .justify_center()
        .rounded(crate::material::radius_card(cx))
        .border_1()
        .border_color(if is_selected {
            cx.theme().primary
        } else {
            cx.theme().border
        })
        .bg(if is_selected {
            cx.theme().accent
        } else {
            cx.theme().background
        })
        .hover(|el| el.bg(cx.theme().accent))
        .cursor_pointer()
        .child(
            div()
                .h(px(68.))
                .flex()
                .items_center()
                .justify_center()
                .child(content),
        )
        .child(
            div()
                .w_full()
                .overflow_hidden()
                .text_secondary(false, cx)
                .child(crate::widgets::fade_text(entry.name.clone())),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            if this.saving {
                return;
            }
            if is_dir {
                this.browse(path.clone(), window, cx);
            } else {
                this.selected = Some(path.clone());
                this.error = None;
                cx.notify();
            }
        }))
        .into_any_element()
    }
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = self.search.read(cx).value().to_lowercase();
        let selected = self.selected.clone();
        let busy = self.saving;
        let parent = self.parent.clone();
        let root = self.project.root.clone();
        let host = self
            .store
            .read(cx)
            .remote_host_name()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::tr!("project_icon.this_machine").into_owned());
        let matches: Rc<Vec<usize>> = Rc::new(
            (0..self.entries.len())
                .filter(|&index| self.entries[index].name.to_lowercase().contains(&search))
                .collect(),
        );
        let columns = ((f32::from(self.grid_width) + TILE_GAP) / (TILE_WIDTH + TILE_GAP))
            .floor()
            .max(1.) as usize;
        // The grid is a uniform_list, which needs a definite height; it shares
        // the dialog with the path and search fields, so it takes what the
        // window leaves instead of DIALOG_LIST_HEIGHT.
        let viewport_height =
            (f32::from(window.viewport_size().height) * 0.9 - 360.).clamp(48., 320.);
        let content = if self.loading {
            crate::material::empty_row(crate::tr!("project_icon.loading"), cx).into_any_element()
        } else if matches.is_empty() {
            crate::material::empty_row(crate::tr!("project_icon.empty"), cx).into_any_element()
        } else {
            let rows = matches.len().div_ceil(columns);
            let list = uniform_list(
                "icon-file-rows",
                rows,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|row| {
                            let tiles =
                                &matches[row * columns..matches.len().min((row + 1) * columns)];
                            h_flex().gap(px(TILE_GAP)).pb(px(TILE_GAP)).children(
                                tiles
                                    .iter()
                                    .map(|&index| this.render_tile(&this.entries[index], cx)),
                            )
                        })
                        .collect()
                }),
            )
            .size_full()
            .track_scroll(&self.grid_scroll);
            let scroll = self.grid_scroll.0.borrow().base_handle.clone();
            div()
                .size_full()
                .child(crate::scroll::page_viewport(
                    "icon-file-grid-bounce",
                    &scroll,
                    list,
                ))
                .when(!window.is_inspector_picking(cx), |grid| {
                    grid.child(Scrollbar::vertical(&self.grid_scroll).id("icon-file-scrollbar"))
                })
                .into_any_element()
        };
        let picker = cx.entity().downgrade();
        v_flex()
            .id("project-icon-picker")
            .debug_selector(|| "project-icon-picker".into())
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(artwork(&self.project, 32.))
                    .child(
                        v_flex()
                            .child(div().text_body(false, cx).child(self.project.name.clone()))
                            .child(div().text_secondary(false, cx).child(host)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("icon-project-root")
                            .ghost()
                            .icon(IconName::Folder)
                            .tooltip(crate::tr!("project_icon.project_folder"))
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.browse(root.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new("icon-parent")
                            .ghost()
                            .icon(IconName::ArrowUp)
                            .tooltip(crate::tr!("project_icon.parent"))
                            .disabled(parent.is_none() || busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(parent) = &parent {
                                    this.browse(parent.clone(), window, cx);
                                }
                            })),
                    )
                    .child(
                        div()
                            .id("icon-path")
                            .debug_selector(|| "icon-path".into())
                            .flex_1()
                            .min_w_0()
                            .child(
                                Input::new(&self.path)
                                    .aria_label(crate::tr!("project_icon.folder"))
                                    .disabled(busy),
                            ),
                    )
                    .child(
                        Button::new("icon-go")
                            .label(crate::tr!("project_icon.go"))
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.browse(
                                    PathBuf::from(this.path.read(cx).value().as_str()),
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .child(Input::new(&self.search).aria_label(crate::tr!("project_icon.search")))
            .child(
                div()
                    .id("icon-file-grid")
                    .h(px(viewport_height))
                    .child(content)
                    .on_prepaint(move |bounds, _, cx| {
                        _ = picker.update(cx, |picker, cx| {
                            if picker.grid_width != bounds.size.width {
                                picker.grid_width = bounds.size.width;
                                cx.notify();
                            }
                        });
                    }),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_secondary(false, cx)
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(
                div().text_secondary(false, cx).child(
                    self.entries
                        .iter()
                        .find(|entry| Some(&entry.path) == selected.as_ref())
                        .map(|entry| entry.name.clone())
                        .unwrap_or_else(|| crate::tr!("project_icon.hint").into_owned()),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .justify_between()
                    .child(
                        Button::new("icon-reset")
                            .ghost()
                            .label(crate::tr!("project_icon.default"))
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save(None, window, cx)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("icon-cancel")
                                    .label(crate::tr!("sidebar.cancel"))
                                    .disabled(busy)
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(
                                Button::new("icon-save")
                                    .primary()
                                    .label(if busy {
                                        crate::tr!("project_icon.saving")
                                    } else {
                                        crate::tr!("project_icon.choose")
                                    })
                                    .disabled(selected.is_none() || busy)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(path) = &selected {
                                            this.save(Some(path.clone()), window, cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};

    struct EmptyView;
    impl Render for EmptyView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn picker_returns_foreign_host_paths_unchanged(cx: &mut TestAppContext) {
        use tcode_client::HostLink;
        use tcode_protocol::{ClientPayload, Query, decode_client_line};
        cx.update(crate::theme::init);
        let (to_host, requests) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let (picker, cx) = cx.add_window_view(|window, cx| {
            Picker::new(
                store.clone(),
                Project::from_root(PathBuf::from("/project")),
                window,
                cx,
            )
        });
        // These are opaque host paths, including the opposite platform's separators.
        for path in [r"Q:\host\images\logo.png", "/host/images/logo.png"] {
            picker.update(cx, |picker, cx| {
                picker.entries = vec![IconImageEntry {
                    path: PathBuf::from(path),
                    name: "logo.png".into(),
                    is_dir: false,
                }];
                cx.notify();
            });
            draw(cx);
            let tile = cx.debug_bounds("icon-file-logo.png").unwrap();
            cx.simulate_click(tile.center(), gpui::Modifiers::default());
            assert_eq!(
                picker.read_with(cx, |picker, _| picker.selected.clone()),
                Some(PathBuf::from(path))
            );
            draw(cx);
            let read = std::iter::from_fn(|| requests.try_recv().ok())
                .filter_map(|line| {
                    let request = decode_client_line(&line).unwrap();
                    match request.payload {
                        ClientPayload::Query(Query::ReadIconImage { path }) => Some(path),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            assert!(
                read.contains(&PathBuf::from(path)),
                "thumbnail must use the host path: {read:?}"
            );
        }
    }

    #[gpui::test]
    fn the_file_grid_builds_only_the_visible_thumbnails(cx: &mut TestAppContext) {
        use tcode_client::HostLink;
        use tcode_protocol::{ClientPayload, Query, decode_client_line};
        cx.update(crate::theme::init);
        let (to_host, requests) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let (picker, cx) = cx.add_window_view(|window, cx| {
            Picker::new(
                store.clone(),
                Project::from_root(PathBuf::from("/project")),
                window,
                cx,
            )
        });
        cx.simulate_resize(size(px(648.), px(700.)));
        draw(cx);
        picker.update(cx, |picker, cx| {
            picker.entries = (0..300)
                .map(|index| IconImageEntry {
                    path: PathBuf::from(format!("/host/images/{index}.png")),
                    name: format!("{index}.png"),
                    is_dir: false,
                })
                .collect();
            cx.notify();
        });
        draw(cx);
        let reads = || {
            std::iter::from_fn(|| requests.try_recv().ok())
                .filter_map(|line| match decode_client_line(&line).unwrap().payload {
                    ClientPayload::Query(Query::ReadIconImage { path }) => Some(path),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let first = cx.debug_bounds("icon-file-0.png").unwrap();
        assert_eq!(
            cx.debug_bounds("icon-file-5.png").unwrap().top(),
            first.top(),
            "the grid fills its width with tiles"
        );
        assert!(
            cx.debug_bounds("icon-file-299.png").is_none(),
            "the grid must not build every tile in the folder"
        );
        let read = reads();
        assert!(read.contains(&PathBuf::from("/host/images/0.png")));
        assert!(
            !read.contains(&PathBuf::from("/host/images/299.png")),
            "an offscreen thumbnail must not be loaded"
        );

        cx.simulate_event(gpui::ScrollWheelEvent {
            position: first.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-100_000.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        });
        draw(cx);
        assert!(cx.debug_bounds("icon-file-299.png").is_some());
        assert!(cx.debug_bounds("icon-file-0.png").is_none());
        assert!(reads().contains(&PathBuf::from("/host/images/299.png")));
    }

    #[gpui::test]
    fn entering_a_folder_keeps_the_picker_open_and_displays_the_host_listing(
        cx: &mut TestAppContext,
    ) {
        use tcode_client::HostLink;
        use tcode_protocol::{ClientPayload, HostMessage, Query, decode_client_line, encode_line};
        cx.update(crate::theme::init);
        let root = PathBuf::from("/host/project");
        let folder = root.join("pictures");
        let project = Project::from_root(root.clone());
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            link.pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let requested_folder = folder.clone();
        let _host = cx.background_executor.spawn(async move {
            while let Ok(line) = requests.recv().await {
                let request = decode_client_line(&line).unwrap();
                let response = match request.payload {
                    ClientPayload::Query(Query::BrowseIconImages { directory }) => {
                        let entries = if directory == root {
                            vec![IconImageEntry {
                                path: requested_folder.clone(),
                                name: "pictures".into(),
                                is_dir: true,
                            }]
                        } else {
                            assert_eq!(directory, requested_folder);
                            vec![IconImageEntry {
                                path: directory.join("logo.png"),
                                name: "logo.png".into(),
                                is_dir: false,
                            }]
                        };
                        QueryResponse::IconImages {
                            parent: directory.parent().map(PathBuf::from),
                            directory,
                            entries,
                        }
                    }
                    ClientPayload::Query(Query::ReadIconImage { .. }) => QueryResponse::FileBytes(
                        include_bytes!("../../../assets/icons/app/tcode.png").to_vec(),
                    ),
                    _ => continue,
                };
                replies
                    .send(
                        encode_line(&HostMessage::QueryResult {
                            id: request.id,
                            result: Ok(response),
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            }
        });
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| EmptyView);
            gpui_base::Root::new(view, window, cx)
        });
        cx.simulate_resize(size(px(390.), px(700.)));
        cx.update(|window, cx| open(store.clone(), project, window, cx));
        draw(cx);
        let path = cx.debug_bounds("icon-path").unwrap();
        cx.simulate_click(path.center(), gpui::Modifiers::default());
        cx.dispatch_action(crate::widgets::input::SelectAll);
        cx.simulate_input(folder.to_str().unwrap());
        cx.simulate_keystrokes("enter");
        draw(cx);
        assert!(
            cx.debug_bounds("project-icon-picker").is_some(),
            "Enter dismissed the picker"
        );
        assert!(cx.debug_bounds("icon-file-logo.png").is_some());
        cx.update(|window, cx| window.close_dialog(cx));
    }
}

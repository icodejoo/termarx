use super::*;
use gpui_kit::{App, Global, Point, WindowHandle};

#[derive(Clone)]
struct WindowTabDrag {
    source: WeakEntity<TerminalView>,
    window: gpui_kit::AnyWindowHandle,
    tab_id: TabId,
    start: Point<Pixels>,
}

#[derive(Default)]
struct WindowTabDragState(Option<WindowTabDrag>);
impl Global for WindowTabDragState {}

struct TransferredTab {
    tab: TerminalTab,
    layout: Option<NativePaneLayoutTree>,
    zoom: Option<NativePaneZoomSnapshot>,
    /// 标签背景色。
    color: Option<tab_colors::TabColor>,
    /// 标签自定义指令。
    commands: Option<Vec<termy_core::session_model::StoredTabCommand>>,
    /// 窗格 id -> 侧表数据；`rebind` 换窗格 id 时跟着改键。
    pane_meta: HashMap<String, titles::source::PaneMeta>,
}

impl TransferredTab {
    fn rebind(&mut self, id: TabId, router: &NativeTerminalWakeupRouter) {
        let mut names = HashMap::new();
        let mut rebind_pane = |pane: &mut TerminalPane| {
            let next = NEXT_NATIVE_TERMINAL_WAKEUP_ID.fetch_add(1, Ordering::Relaxed);
            let name = format!("%native-transfer-{next}");
            names.insert(pane.id.clone(), name.clone());
            pane.id = name;
            pane.cached_element_ids = PaneCachedElementIds::new(&pane.id);
            pane.render_cache.borrow_mut().clear();
            if let Terminal::Native(native) = &pane.terminal {
                *native
                    .wakeup_route
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(router.clone());
                router.mark_ready(native.wakeup_id);
            }
        };
        for pane in &mut self.tab.panes {
            rebind_pane(pane);
        }
        if let Some(zoom) = &mut self.zoom {
            for pane in &mut zoom.other_panes {
                rebind_pane(pane);
            }
        }
        fn rename_tree(node: &mut NativePaneLayoutNode, names: &HashMap<String, String>) {
            match node {
                NativePaneLayoutNode::Leaf { pane_id } => {
                    if let Some(name) = names.get(pane_id) {
                        *pane_id = name.clone();
                    }
                }
                NativePaneLayoutNode::Split { first, second, .. } => {
                    rename_tree(first, names);
                    rename_tree(second, names);
                }
            }
        }
        if let Some(name) = names.get(&self.tab.active_pane_id) {
            self.tab.active_pane_id = name.clone();
        }
        if let Some(layout) = &mut self.layout {
            rename_tree(&mut layout.root, &names);
        }
        if let Some(zoom) = &mut self.zoom {
            if let Some(name) = names.get(&zoom.active_pane_id) {
                zoom.active_pane_id = name.clone();
            }
            if let Some(layout) = &mut zoom.layout_tree {
                rename_tree(&mut layout.root, &names);
            }
        }
        self.pane_meta = std::mem::take(&mut self.pane_meta)
            .into_iter()
            .map(|(old, meta)| (names.get(&old).cloned().unwrap_or(old), meta))
            .collect();
        self.tab.id = id;
        self.tab.window_id = format!("@native-{id}");
    }
}

impl TerminalView {
    pub(crate) fn arm_window_tab_drag(
        &self,
        index: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.session.tabs.get(index) {
            log::debug!("Arming tab drag for tab {}", tab.id);
            let source = cx.entity().downgrade();
            cx.set_global(WindowTabDragState(Some(WindowTabDrag {
                source,
                window: self.window_handle,
                tab_id: tab.id,
                start: position,
            })));
        }
    }

    pub(crate) fn cancel_window_tab_drag(&mut self, cx: &mut Context<Self>) -> bool {
        let active = cx
            .try_global::<WindowTabDragState>()
            .is_some_and(|state| state.0.is_some());
        if active {
            cx.set_global(WindowTabDragState::default());
            self.finish_tab_drag();
            cx.notify();
        }
        active
    }

    pub(crate) fn finish_window_tab_drag(
        &mut self,
        event: &MouseUpEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.button != MouseButton::Left {
            return false;
        }
        let Some(drag) = cx
            .try_global::<WindowTabDragState>()
            .and_then(|state| state.0.clone())
        else {
            return false;
        };
        cx.set_global(WindowTabDragState::default());
        let same_window = drag.window == self.window_handle;
        log::debug!(
            "Releasing tab drag at {:?}; source window: {same_window}",
            event.position
        );
        let viewport = Bounds::new(gpui_kit::point(px(0.0), px(0.0)), window.viewport_size());
        if same_window && viewport.contains(&event.position) {
            return false;
        }
        if same_window && (event.position - drag.start).magnitude() < 6.0 {
            return false;
        }
        self.finish_tab_drag();
        cx.notify();
        let explicit_target = (!same_window).then_some(self.window_handle);
        let screen_position = window.bounds().origin + event.position;
        cx.spawn(async move |_, cx: &mut AsyncApp| {
            // Wayland's implicit pointer grab ends on release. Let the compositor
            // report the newly hovered surface before choosing the destination;
            // no global window coordinates are available there.
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            cx.update(|cx| {
                let destination = explicit_target.or_else(|| {
                    cx.windows()
                        .into_iter()
                        .filter(|handle| *handle != drag.window)
                        .filter_map(|handle| handle.downcast::<TerminalView>())
                        .find_map(|handle| {
                            handle
                                .update(cx, |_, window, _| {
                                    if cfg!(target_os = "macos") {
                                        window.bounds().contains(&screen_position)
                                    } else {
                                        window.is_window_hovered()
                                    }
                                })
                                .ok()
                                .filter(|hovered| *hovered)
                                .map(|_| handle.into())
                        })
                });
                if let Err(error) = Self::transfer_dragged_tab(&drag, destination, cx) {
                    log::warn!("Could not move tab between windows: {error}");
                    crate::ui::toast::error(error);
                }
            });
        })
        .detach();
        true
    }

    fn transfer_dragged_tab(
        drag: &WindowTabDrag,
        destination: Option<gpui_kit::AnyWindowHandle>,
        cx: &mut App,
    ) -> Result<(), String> {
        let source = drag
            .source
            .upgrade()
            .ok_or(t!("The source window was closed"))?;
        let native = source.read(cx).runtime_kind() == RuntimeKind::Native;
        if source.read(cx).tab_index_by_id(drag.tab_id).is_none() {
            return Ok(());
        }
        let mut created_window = false;
        let target: WindowHandle<Self> = if let Some(destination) = destination {
            let handle = destination
                .downcast::<Self>()
                .ok_or(t!("Drop onto a terminal window"))?;
            let compatible = handle
                .update(cx, |view, _, _| {
                    (view.runtime_kind() == RuntimeKind::Native) == native
                })
                .map_err(|error| error.to_string())?;
            if !compatible {
                return Err(
                    t!("Move this tab into a window using the same terminal runtime").into(),
                );
            }
            handle
        } else {
            let mut error = None;
            let mut config =
                crate::config::load_runtime_config(&mut error, "Failed to load config").config;
            config.tmux_enabled = !native;
            config.tmux_persistence = false;
            created_window = true;
            crate::open_terminal_window(cx, config, native)?
        };
        if !native {
            let result = target
                .update(cx, |view, _, cx| {
                    if !view.runtime_uses_tmux() {
                        return Err(t!("The new window could not start tmux").to_string());
                    }
                    let source = source.read(cx);
                    let index = source
                        .tab_index_by_id(drag.tab_id)
                        .ok_or(t!("The source tab was closed"))?;
                    view.tmux_runtime()
                        .client
                        .move_window_from(
                            &source.tmux_runtime().client,
                            &source.session.tabs[index].window_id,
                        )
                        .map_err(|error| error.to_string())?;
                    if created_window {
                        for tab in &view.session.tabs {
                            if let Err(error) =
                                view.tmux_runtime().client.kill_window(&tab.window_id)
                            {
                                log::warn!(
                                    "Could not remove temporary tmux bootstrap tab: {error}"
                                );
                            }
                        }
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            if let Err(error) = result {
                if created_window {
                    let _ = target.update(cx, |_, window, _| window.remove_window());
                }
                return Err(error);
            }
            if created_window {
                let _ = target.update(cx, |view, _, _| view.session.tabs.clear());
            }
        }
        let mut moved = source.update(cx, |view, cx| view.take_tab_for_transfer(drag.tab_id, cx));
        let Some(_) = moved else {
            if created_window {
                let _ = target.update(cx, |_, window, _| window.remove_window());
            }
            return Ok(());
        };
        let result = target.update(cx, |view, window, cx| {
            view.receive_transferred_tab(
                moved
                    .take()
                    .expect("tab remains owned until destination accepts it"),
                cx,
            );
            view.focus_terminal_after_tab_activation(window, cx);
            window.activate_window();
        });
        if let Err(error) = result {
            if let Some(moved) = moved {
                source.update(cx, |view, cx| view.receive_transferred_tab(moved, cx));
            }
            return Err(error.to_string());
        }
        let (close_source, transfer_persistence) = source.update(cx, |view, cx| {
            let empty = view.session.tabs.is_empty() && !view.has_other_workspaces();
            let owns = empty && view.owns_persisted_session;
            if owns {
                view.owns_persisted_session = false;
            }
            view.sync_persisted_native_workspace();
            if !empty && view.runtime_uses_tmux() {
                view.refresh_tmux_snapshot();
            }
            cx.notify();
            (empty, owns)
        });
        if transfer_persistence {
            let _ = target.update(cx, |view, _, _| {
                view.owns_persisted_session = true;
                view.sync_persisted_native_workspace();
            });
        }
        if close_source {
            let _ = drag
                .window
                .update(cx, |_, window, _| window.remove_window());
        }
        Ok(())
    }

    fn take_tab_for_transfer(
        &mut self,
        tab_id: TabId,
        cx: &mut Context<Self>,
    ) -> Option<TransferredTab> {
        let index = self.tab_index_by_id(tab_id)?;
        let pane_ids = self.session.tabs[index]
            .panes
            .iter()
            .map(|pane| pane.id.clone())
            .collect::<Vec<_>>();
        let _ = self.release_forwarded_mouse_presses_for_panes(&pane_ids);
        let tab = self.session.tabs.remove(index);
        let layout = self.session.native_pane_layout_trees.remove(&tab_id);
        let zoom = self.session.native_pane_zoom_snapshots.remove(&tab_id);
        // 标签颜色与各窗格的手动名/标题/目录跟着标签走，缩放时藏起来的窗格也要带上。
        let color = self.tab_colors.remove(&tab_id);
        let commands = self.tab_commands.remove(&tab_id);
        let pane_meta = tab
            .panes
            .iter()
            .chain(zoom.iter().flat_map(|zoom| zoom.other_panes.iter()))
            .filter_map(|pane| {
                let meta = self.pane_meta.remove(&pane.id)?;
                Some((pane.id.clone(), meta))
            })
            .collect();
        self.sync_git_watch(cx);
        if self.session.active_tab > index {
            self.session.active_tab -= 1;
        }
        self.session.active_tab = self
            .session
            .active_tab
            .min(self.session.tabs.len().saturating_sub(1));
        self.reset_tab_interaction_state();
        self.mark_tab_strip_layout_dirty();
        self.sync_native_terminal_wakeup_interest();
        self.last_terminal_resize_signature = None;
        cx.notify();
        Some(TransferredTab {
            tab,
            layout,
            zoom,
            color,
            commands,
            pane_meta,
        })
    }

    fn receive_transferred_tab(&mut self, mut moved: TransferredTab, cx: &mut Context<Self>) {
        let id = self.allocate_tab_id();
        if self.runtime_kind() == RuntimeKind::Native {
            moved.rebind(id, &self.native_terminal_wakeup_router);
        } else {
            moved.tab.id = id;
        }
        let tmux_window_id = moved.tab.window_id.clone();
        if let Some(layout) = moved.layout {
            self.session.native_pane_layout_trees.insert(id, layout);
        }
        if let Some(zoom) = moved.zoom {
            self.session.native_pane_zoom_snapshots.insert(id, zoom);
        }
        if let Some(color) = moved.color {
            self.tab_colors.insert(id, color);
        }
        if let Some(commands) = moved.commands {
            self.tab_commands.insert(id, commands);
        }
        self.pane_meta.extend(moved.pane_meta);
        self.session.tabs.push(moved.tab);
        self.session.active_tab = self.session.tabs.len() - 1;
        self.sync_git_watch(cx);
        if self.runtime_uses_tmux() {
            if let Err(error) = self.tmux_runtime().client.select_window(&tmux_window_id) {
                log::warn!("Could not select moved tmux tab: {error}");
            }
            self.refresh_tmux_snapshot();
        }
        self.reset_tab_interaction_state();
        self.refresh_tab_title(self.session.active_tab);
        self.mark_tab_strip_layout_dirty();
        self.sync_native_terminal_wakeup_interest();
        self.last_terminal_resize_signature = None;
        self.schedule_persist_native_workspace(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, TestAppContext, WindowOptions};

    fn test_window(cx: &mut App, tabs: usize) -> WindowHandle<TerminalView> {
        cx.open_window(WindowOptions::default(), |window, cx| {
            cx.new(|cx| {
                let config = AppConfig {
                    tmux_enabled: false,
                    native_tab_persistence: false,
                    sidebar_enabled: false,
                    ..AppConfig::default()
                };
                let mut view = TerminalView::new_for_window(window, cx, config, true);
                for _ in 0..tabs {
                    let id = view.allocate_tab_id();
                    let tab = TerminalView::create_native_tab(
                        id,
                        Terminal::new_test_display(TerminalSize::default()),
                        80,
                        24,
                        None,
                    );
                    view.session.tabs.push(tab);
                }
                view
            })
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn transfer_preserves_live_tab_state_and_remaps_colliding_ids(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let source = test_window(cx, 2);
            let target = test_window(cx, 1);
            let source_view = source.entity(cx).unwrap();
            // 测试调度器要求确定性：不创建后台 git 监听线程。
            for window in [source, target] {
                window
                    .update(cx, |view, _, _| {
                        view.git_watch = titles::git::GitWatch::Unavailable;
                    })
                    .unwrap();
            }
            source
                .update(cx, |view, _, _| {
                    let tab = &mut view.session.tabs[0];
                    tab.pinned = true;
                    tab.manual_title = Some("running job".into());
                    tab.last_prompt_cwd = Some("/tmp/project".into());
                    tab.running_process = true;
                    let pane_id = tab.panes[0].id.clone();
                    let tab_id = tab.id;
                    view.tab_colors.insert(tab_id, tab_colors::TabColor::Blue);
                    view.pane_meta.insert(
                        pane_id.clone(),
                        titles::source::PaneMeta {
                            manual_title: Some("build".to_string()),
                            reported_title: Some("vim main.rs".to_string()),
                            cwd: Some("/tmp/project".to_string()),
                            ..Default::default()
                        },
                    );
                    view.session.native_pane_layout_trees.insert(
                        tab.id,
                        NativePaneLayoutTree {
                            root: NativePaneLayoutNode::Leaf { pane_id },
                        },
                    );
                })
                .unwrap();
            let drag = WindowTabDrag {
                source: source_view.downgrade(),
                window: source.into(),
                tab_id: 1,
                start: gpui_kit::point(px(0.0), px(0.0)),
            };
            TerminalView::transfer_dragged_tab(&drag, Some(target.into()), cx).unwrap();
            assert_eq!(source_view.read(cx).session.tabs.len(), 1);
            target
                .update(cx, |view, _, _| {
                    assert_eq!(view.session.tabs.len(), 2);
                    let moved = &view.session.tabs[1];
                    assert_ne!(moved.id, view.session.tabs[0].id);
                    assert_ne!(moved.panes[0].id, view.session.tabs[0].panes[0].id);
                    assert_eq!(moved.manual_title.as_deref(), Some("running job"));
                    assert_eq!(moved.last_prompt_cwd.as_deref(), Some("/tmp/project"));
                    assert!(moved.pinned && moved.running_process);
                    assert_eq!(
                        view.session.native_pane_layout_trees[&moved.id].root,
                        NativePaneLayoutNode::Leaf {
                            pane_id: moved.panes[0].id.clone()
                        }
                    );
                    assert_eq!(view.session.active_tab, 1);
                    // 标签颜色与窗格侧表跟着标签迁移，并改用新的 id。
                    let new_pane_id = moved.panes[0].id.as_str();
                    assert_eq!(
                        view.tab_colors.get(&moved.id),
                        Some(&tab_colors::TabColor::Blue)
                    );
                    let meta = &view.pane_meta[new_pane_id];
                    assert_eq!(meta.manual_title.as_deref(), Some("build"));
                    assert_eq!(meta.reported_title.as_deref(), Some("vim main.rs"));
                    assert_eq!(meta.cwd.as_deref(), Some("/tmp/project"));
                })
                .unwrap();
            source
                .update(cx, |view, _, _| {
                    assert!(!view.tab_colors.contains_key(&1));
                    assert!(view.pane_meta.is_empty());
                })
                .unwrap();
        });
    }

    #[gpui_kit::test]
    fn moving_last_tab_closes_source_and_hands_off_persistence(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let source = test_window(cx, 1);
            let target = test_window(cx, 1);
            let source_view = source.entity(cx).unwrap();
            assert!(source_view.read(cx).owns_persisted_session);
            let drag = WindowTabDrag {
                source: source_view.downgrade(),
                window: source.into(),
                tab_id: 1,
                start: gpui_kit::point(px(0.0), px(0.0)),
            };
            TerminalView::transfer_dragged_tab(&drag, Some(target.into()), cx).unwrap();
            target
                .update(cx, |view, _, _| assert!(view.owns_persisted_session))
                .unwrap();
            assert!(!source_view.read(cx).owns_persisted_session);
        });
        assert_eq!(cx.windows().len(), 1);
    }

    /// 监听器创建失败（`GitWatch::Unavailable`）后，提示符到来既不要求重新对齐，也不会再建监听器。
    #[gpui_kit::test]
    fn unavailable_git_watch_is_never_retried(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let window = test_window(cx, 1);
            window
                .update(cx, |view, _, cx| {
                    view.git_watch = titles::git::GitWatch::Unavailable;
                    view.record_pane_cwd("%native-pane-1", r"E:\work");
                    assert!(!view.git_watch_needs_sync("%native-pane-1"));
                    view.sync_git_watch(cx);
                    assert!(matches!(view.git_watch, titles::git::GitWatch::Unavailable));
                })
                .unwrap();
        });
    }
}

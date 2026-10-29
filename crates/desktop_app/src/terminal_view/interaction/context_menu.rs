use super::*;
#[cfg(target_os = "macos")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

#[derive(Clone, Debug, PartialEq)]
pub(in crate::terminal_view) struct TerminalContextMenuState {
    pub(in crate::terminal_view) anchor_position: gpui_kit::Point<Pixels>,
    pub(in crate::terminal_view) buffer_position: Option<SelectionPos>,
    pub(in crate::terminal_view) image: Option<KittyImageSelection>,
    pub(in crate::terminal_view) selected_text: Option<String>,
    pub(in crate::terminal_view) can_copy: bool,
    pub(in crate::terminal_view) can_paste: bool,
    /// 从窗格标题栏的下拉按钮打开时，记录目标窗格 id；菜单会多出针对它的操作项。
    pub(in crate::terminal_view) pane_actions_for: Option<String>,
}

/// 窗格下拉菜单里针对窗格的操作（macOS 走原生菜单，不会构造）。
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::terminal_view) enum PaneMenuAction {
    /// 发送 Ctrl+C 中断前台任务。
    Interrupt,
    /// 强制结束 shell 下的全部子进程。
    ForceStop,
    /// 和指定方向的相邻窗格交换位置。
    Move(NativeFocusDirection),
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::terminal_view) struct TabContextMenuState {
    pub(in crate::terminal_view) anchor_position: gpui_kit::Point<Pixels>,
    pub(in crate::terminal_view) tab_id: TabId,
    pub(in crate::terminal_view) pinned: bool,
}

impl TerminalView {
    fn terminal_context_menu_buffer_position(
        &self,
        position: gpui_kit::Point<Pixels>,
    ) -> Option<SelectionPos> {
        let (_, buffer_position) = self.position_to_pane_selection_pos(position, false)?;
        Some(buffer_position)
    }

    pub(in super::super) fn format_terminal_buffer_position(position: SelectionPos) -> String {
        t!(
            "Buffer Position: Line {line}, Column {column}",
            line = position.line,
            column = position.col
        )
    }

    #[cfg(any(test, target_os = "macos"))]
    fn copyable_terminal_buffer_position(position: SelectionPos) -> String {
        format!("line={},col={}", position.line, position.col)
    }

    fn terminal_context_menu_capabilities(&self, cx: &mut Context<Self>) -> (Option<String>, bool) {
        let selected_text = self.selected_text();
        let can_paste = self
            .active_terminal()
            .is_some_and(Terminal::kitty_clipboard_paste_events_enabled)
            && crate::native_sdk::available_clipboard_formats().is_ok()
            || cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some();
        (selected_text, can_paste)
    }

    #[cfg(any(test, target_os = "macos"))]
    fn command_action_for_context_menu_action(
        action: crate::native_sdk::ContextMenuAction,
    ) -> Option<CommandAction> {
        match action {
            crate::native_sdk::ContextMenuAction::Copy => Some(CommandAction::Copy),
            crate::native_sdk::ContextMenuAction::Paste => Some(CommandAction::Paste),
            crate::native_sdk::ContextMenuAction::OpenSearch => Some(CommandAction::OpenSearch),
            crate::native_sdk::ContextMenuAction::CopyImage
            | crate::native_sdk::ContextMenuAction::CopyBufferPosition => None,
        }
    }

    #[cfg(target_os = "macos")]
    fn native_context_menu_anchor(
        window: &Window,
        position: gpui_kit::Point<Pixels>,
    ) -> Option<crate::native_sdk::NativeContextMenuAnchor> {
        let raw_handle = HasWindowHandle::window_handle(window).ok()?.as_raw();
        let native_view = match raw_handle {
            RawWindowHandle::AppKit(handle) => handle.ns_view.as_ptr() as usize,
            _ => return None,
        };

        let x: f32 = position.x.into();
        let y: f32 = position.y.into();

        Some(crate::native_sdk::NativeContextMenuAnchor {
            native_view,
            x: x as f64,
            y: y as f64,
        })
    }

    pub(in super::super) fn close_terminal_context_menu(&mut self, cx: &mut Context<Self>) -> bool {
        if self.terminal_context_menu.take().is_some() {
            self.notify_overlay(cx);
            true
        } else {
            false
        }
    }

    pub(in super::super) fn close_tab_context_menu(&mut self, cx: &mut Context<Self>) -> bool {
        if self.tab_context_menu.take().is_some() {
            self.notify_overlay(cx);
            true
        } else {
            false
        }
    }

    pub(in super::super) fn execute_terminal_context_menu_command(
        &mut self,
        action: CommandAction,
        cx: &mut Context<Self>,
    ) {
        if !matches!(action, CommandAction::Copy | CommandAction::Paste) {
            return;
        }
        let selected_text = self
            .terminal_context_menu
            .as_ref()
            .and_then(|state| state.selected_text.clone());
        let _ = self.close_terminal_context_menu(cx);
        if action == CommandAction::Copy {
            if let Some(selected_text) = selected_text {
                cx.write_to_clipboard(ClipboardItem::new_string(selected_text));
            }
        } else {
            let _ = self.execute_input_command_action(action, cx);
        }
    }

    #[cfg(target_os = "macos")]
    fn execute_terminal_context_menu_action(
        &mut self,
        action: crate::native_sdk::ContextMenuAction,
        cx: &mut Context<Self>,
    ) {
        if let Some(command_action) = Self::command_action_for_context_menu_action(action) {
            if command_action == CommandAction::OpenSearch {
                let _ = self.close_terminal_context_menu(cx);
                self.open_search(cx);
            } else {
                self.execute_terminal_context_menu_command(command_action, cx);
            }
            return;
        }

        match action {
            crate::native_sdk::ContextMenuAction::CopyImage => {
                self.execute_terminal_context_menu_copy_image(cx);
            }
            crate::native_sdk::ContextMenuAction::CopyBufferPosition => {
                self.execute_terminal_context_menu_copy_buffer_position(cx);
            }
            _ => {}
        }
    }

    pub(in super::super) fn execute_terminal_context_menu_copy_image(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let _ = self.clear_stale_kitty_image_state();
        let selection = self
            .terminal_context_menu
            .as_ref()
            .and_then(|state| state.image.as_ref())
            .cloned();
        let Some(png) = selection
            .as_ref()
            .and_then(|selection| self.current_kitty_image_placement(selection))
            .map(|placement| placement.image.png().clone())
        else {
            let _ = self.close_terminal_context_menu(cx);
            return;
        };

        let _ = self.close_terminal_context_menu(cx);
        cx.write_to_clipboard(super::input::kitty_png_clipboard_item(png.as_ref()));
        crate::ui::toast::success(t!("Copied image"));
        self.notify_overlay(cx);
    }

    #[cfg(target_os = "macos")]
    fn execute_terminal_context_menu_copy_buffer_position(&mut self, cx: &mut Context<Self>) {
        let Some(position) = self
            .terminal_context_menu
            .as_ref()
            .and_then(|state| state.buffer_position)
        else {
            let _ = self.close_terminal_context_menu(cx);
            return;
        };

        let _ = self.close_terminal_context_menu(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(
            Self::copyable_terminal_buffer_position(position),
        ));
        crate::ui::toast::success(t!("Copied buffer position"));
        self.notify_overlay(cx);
    }

    #[cfg(target_os = "macos")]
    fn execute_tab_context_menu_action(
        &mut self,
        action: crate::native_sdk::TabContextMenuAction,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_id) = self.tab_context_menu.as_ref().map(|state| state.tab_id) else {
            let _ = self.close_tab_context_menu(cx);
            return;
        };

        let _ = self.close_tab_context_menu(cx);
        match action {
            crate::native_sdk::TabContextMenuAction::Rename => {
                if let Some(index) = self.tab_index_by_id(tab_id) {
                    self.begin_rename_tab(index, cx);
                }
            }
            crate::native_sdk::TabContextMenuAction::Duplicate => {
                let _ = self.duplicate_tab_by_id(tab_id, cx);
            }
            crate::native_sdk::TabContextMenuAction::Pin => {
                let _ = self.set_tab_pinned_by_id(tab_id, true, cx);
            }
            crate::native_sdk::TabContextMenuAction::Unpin => {
                let _ = self.set_tab_pinned_by_id(tab_id, false, cx);
            }
            crate::native_sdk::TabContextMenuAction::Close => {
                if let Some(index) = self.tab_index_by_id(tab_id) {
                    self.close_tab(index, cx);
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn schedule_native_terminal_context_menu(
        &mut self,
        buffer_position_label: Option<String>,
        can_copy: bool,
        can_copy_image: bool,
        can_paste: bool,
        anchor: Option<crate::native_sdk::NativeContextMenuAnchor>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let action = cx
                .background_executor()
                .spawn(async move {
                    crate::native_sdk::show_copy_paste_context_menu(
                        buffer_position_label,
                        can_copy,
                        can_copy_image,
                        can_paste,
                        anchor,
                    )
                })
                .await;

            let _ = cx.update(|cx| {
                this.update(cx, |view, cx| {
                    let Some(action) = action else {
                        let _ = view.close_terminal_context_menu(cx);
                        return;
                    };
                    view.execute_terminal_context_menu_action(action, cx);
                })
            });
        })
        .detach();
    }

    #[cfg(target_os = "macos")]
    fn schedule_native_tab_context_menu(
        &mut self,
        pinned: bool,
        anchor: Option<crate::native_sdk::NativeContextMenuAnchor>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let action = cx
                .background_executor()
                .spawn(async move { crate::native_sdk::show_tab_context_menu(pinned, anchor) })
                .await;

            let _ = cx.update(|cx| {
                this.update(cx, |view, cx| {
                    let Some(action) = action else {
                        let _ = view.close_tab_context_menu(cx);
                        return;
                    };
                    view.execute_tab_context_menu_action(action, cx);
                })
            });
        })
        .detach();
    }

    #[cfg(not(target_os = "macos"))]
    pub(in super::super) fn open_terminal_context_menu(
        &mut self,
        position: gpui_kit::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_terminal_context_menu_with_native_anchor(position, None, cx);
    }

    /// 把窗口坐标处的窗格设为活动窗格（已是活动窗格则不动）。
    ///
    /// - `position`：鼠标位置（窗口坐标）。
    ///
    /// 右键菜单的 Close/Rename Pane 等命令都作用在活动窗格上，打开菜单前必须先调用。
    pub(in super::super) fn focus_pane_at_position(
        &mut self,
        position: gpui_kit::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some((pane_id, _)) = self.position_to_pane_cell(position, false)
            && !self.is_active_pane_id(pane_id.as_str())
        {
            let _ = self.focus_pane_target(pane_id.as_str(), cx);
        }
    }

    /// 从窗格标题栏的下拉按钮打开窗格菜单：先激活该窗格，再弹出带窗格操作项的菜单。
    ///
    /// - `pane_id`：被点击的窗格。
    /// - `position`：按钮点击位置（窗口坐标），菜单从这里展开。
    /// - `window`：当前窗口（macOS 原生菜单定位用）。
    pub(in super::super) fn open_pane_menu_from_button(
        &mut self,
        pane_id: &str,
        position: gpui_kit::Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_active_pane_id(pane_id) {
            let _ = self.focus_pane_target(pane_id, cx);
        }
        self.open_terminal_context_menu_for_window(position, window, cx);
        if let Some(state) = self.terminal_context_menu.as_mut() {
            state.pane_actions_for = Some(pane_id.to_string());
            self.notify_overlay(cx);
        }
    }

    /// 执行窗格菜单项：先关菜单，再对菜单记录的目标窗格生效。
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(in super::super) fn execute_pane_menu_action(
        &mut self,
        action: PaneMenuAction,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = self
            .terminal_context_menu
            .as_ref()
            .and_then(|state| state.pane_actions_for.clone())
        else {
            return;
        };
        let _ = self.close_terminal_context_menu(cx);
        match action {
            PaneMenuAction::Interrupt => {
                self.interrupt_pane_task(pane_id.as_str());
            }
            PaneMenuAction::ForceStop => {
                self.force_stop_pane_task(pane_id.as_str());
            }
            PaneMenuAction::Move(direction) => {
                self.move_pane_in_direction(pane_id.as_str(), direction, cx);
            }
        }
    }

    pub(in super::super) fn open_terminal_context_menu_for_window(
        &mut self,
        position: gpui_kit::Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_os = "macos")]
        let native_anchor = Self::native_context_menu_anchor(window, position);
        #[cfg(not(target_os = "macos"))]
        let native_anchor = {
            let _ = window;
            None
        };

        self.open_terminal_context_menu_with_native_anchor(position, native_anchor, cx);
    }

    fn open_terminal_context_menu_with_native_anchor(
        &mut self,
        position: gpui_kit::Point<Pixels>,
        native_anchor: Option<crate::native_sdk::NativeContextMenuAnchor>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_os = "macos"))]
        self.schedule_plugin_refresh(cx);
        let _ = self.close_tab_context_menu(cx);
        let (selected_text, can_paste) = self.terminal_context_menu_capabilities(cx);
        let can_copy = selected_text.is_some();
        let buffer_position = self.terminal_context_menu_buffer_position(position);
        let image = self.kitty_image_at_position(position);
        #[cfg(target_os = "macos")]
        let can_copy_image = image.is_some();
        let state = TerminalContextMenuState {
            anchor_position: position,
            buffer_position,
            image,
            selected_text,
            can_copy,
            can_paste,
            pane_actions_for: None,
        };
        #[cfg(not(target_os = "macos"))]
        let state_changed = self.terminal_context_menu.as_ref() != Some(&state);
        self.terminal_context_menu = Some(state);

        #[cfg(not(target_os = "macos"))]
        {
            let _ = native_anchor;
            if state_changed {
                self.notify_overlay(cx);
            }
        }

        #[cfg(target_os = "macos")]
        {
            let _ = position;
            let buffer_position_label = buffer_position.map(Self::format_terminal_buffer_position);
            self.schedule_native_terminal_context_menu(
                buffer_position_label,
                can_copy,
                can_copy_image,
                can_paste,
                native_anchor,
                cx,
            );
        }
    }

    pub(in super::super) fn open_tab_context_menu_for_window(
        &mut self,
        tab_index: usize,
        position: gpui_kit::Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_os = "macos")]
        let native_anchor = Self::native_context_menu_anchor(window, position);
        #[cfg(not(target_os = "macos"))]
        let native_anchor = {
            let _ = window;
            None
        };

        self.open_tab_context_menu_with_native_anchor(tab_index, position, native_anchor, cx);
    }

    fn open_tab_context_menu_with_native_anchor(
        &mut self,
        tab_index: usize,
        position: gpui_kit::Point<Pixels>,
        native_anchor: Option<crate::native_sdk::NativeContextMenuAnchor>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_os = "macos"))]
        self.schedule_plugin_refresh(cx);
        let Some((tab_id, pinned)) = self
            .session
            .tabs
            .get(tab_index)
            .map(|tab| (tab.id, tab.pinned))
        else {
            return;
        };

        let _ = self.close_terminal_context_menu(cx);
        let state = TabContextMenuState {
            anchor_position: position,
            tab_id,
            pinned,
        };
        #[cfg(not(target_os = "macos"))]
        let state_changed = self.tab_context_menu.as_ref() != Some(&state);
        self.tab_context_menu = Some(state);

        #[cfg(not(target_os = "macos"))]
        {
            let _ = native_anchor;
            if state_changed {
                self.notify_overlay(cx);
            }
        }

        #[cfg(target_os = "macos")]
        {
            self.schedule_native_tab_context_menu(pinned, native_anchor, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_menu_action_maps_to_command_action() {
        assert_eq!(
            TerminalView::command_action_for_context_menu_action(
                crate::native_sdk::ContextMenuAction::Copy
            ),
            Some(CommandAction::Copy)
        );
        assert_eq!(
            TerminalView::command_action_for_context_menu_action(
                crate::native_sdk::ContextMenuAction::Paste
            ),
            Some(CommandAction::Paste)
        );
        assert_eq!(
            TerminalView::command_action_for_context_menu_action(
                crate::native_sdk::ContextMenuAction::OpenSearch
            ),
            Some(CommandAction::OpenSearch)
        );
        assert_eq!(
            TerminalView::command_action_for_context_menu_action(
                crate::native_sdk::ContextMenuAction::CopyImage
            ),
            None
        );
        assert_eq!(
            TerminalView::command_action_for_context_menu_action(
                crate::native_sdk::ContextMenuAction::CopyBufferPosition
            ),
            None
        );
    }

    #[test]
    fn buffer_position_label_uses_terminal_coordinates() {
        assert_eq!(
            TerminalView::format_terminal_buffer_position(SelectionPos { col: 12, line: -3 }),
            "Buffer Position: Line -3, Column 12"
        );
        assert_eq!(
            TerminalView::copyable_terminal_buffer_position(SelectionPos { col: 12, line: -3 }),
            "line=-3,col=12"
        );
    }
}

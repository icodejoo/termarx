//! 快捷键弹窗：点击顶栏左侧 logo 打开，列表左侧是用途，右侧是当前生效的快捷键。
//!
//! 快捷键从系统按键绑定实时取（含用户在配置里的改动），没有绑定的命令不显示。
//! 交互与“发行说明”弹窗一致：点空白处或按 Esc 关闭，打开时接管键盘输入。

use super::*;
use crate::commands::CommandAction;
use gpui_kit::FontWeight;

/// 一个分组的快捷键行：（分组标题，[(用途, 命令, 快捷键)]）。
type ShortcutSectionRows = (&'static str, Vec<(&'static str, CommandAction, String)>);

/// 快捷键文档的一个分组。
struct ShortcutSection {
    /// 分组标题。
    title: &'static str,
    /// 分组内的条目：（中文用途，命令）。
    items: &'static [(&'static str, CommandAction)],
}

/// 快捷键文档的内容与顺序。
const SHORTCUT_SECTIONS: &[ShortcutSection] = &[
    ShortcutSection {
        title: "Tabs",
        items: &[
            ("New Tab", CommandAction::NewTab),
            ("Close Tab", CommandAction::CloseTab),
            ("Close Pane or Tab", CommandAction::ClosePaneOrTab),
            ("Switch to Left Tab", CommandAction::SwitchTabLeft),
            ("Switch to Right Tab", CommandAction::SwitchTabRight),
            ("Cycle Tabs", CommandAction::CycleTabs),
            ("Move Tab Left", CommandAction::MoveTabLeft),
            ("Move Tab Right", CommandAction::MoveTabRight),
            ("Rename Tab", CommandAction::RenameTab),
            ("Show / Hide Tab Bar", CommandAction::ToggleTabBarVisibility),
        ],
    },
    ShortcutSection {
        title: "Panes",
        items: &[
            ("Split Pane Vertically", CommandAction::SplitPaneVertical),
            (
                "Split Pane Horizontally",
                CommandAction::SplitPaneHorizontal,
            ),
            ("Close Current Pane", CommandAction::ClosePane),
            ("Focus Pane Left", CommandAction::FocusPaneLeft),
            ("Focus Pane Right", CommandAction::FocusPaneRight),
            ("Focus Pane Up", CommandAction::FocusPaneUp),
            ("Focus Pane Down", CommandAction::FocusPaneDown),
            ("Next Pane", CommandAction::FocusPaneNext),
            ("Previous Pane", CommandAction::FocusPanePrevious),
            ("Resize Pane Left", CommandAction::ResizePaneLeft),
            ("Resize Pane Right", CommandAction::ResizePaneRight),
            ("Resize Pane Up", CommandAction::ResizePaneUp),
            ("Resize Pane Down", CommandAction::ResizePaneDown),
            ("Zoom / Restore Current Pane", CommandAction::TogglePaneZoom),
        ],
    },
    ShortcutSection {
        title: "Edit & View",
        items: &[
            ("Copy", CommandAction::Copy),
            ("Paste", CommandAction::Paste),
            ("Select All", CommandAction::SelectAll),
            ("Find", CommandAction::OpenSearch),
            ("Clear Screen", CommandAction::ClearScreen),
            ("Increase Font Size", CommandAction::ZoomIn),
            ("Decrease Font Size", CommandAction::ZoomOut),
            ("Reset Font Size", CommandAction::ZoomReset),
            (
                "Toggle Workspace Sidebar",
                CommandAction::ToggleWorkspaceSidebar,
            ),
            ("Toggle Inspector", CommandAction::ToggleInspector),
        ],
    },
    ShortcutSection {
        title: "App",
        items: &[
            ("Command Palette", CommandAction::ToggleCommandPalette),
            ("Open Settings", CommandAction::OpenSettings),
            ("Open Settings File", CommandAction::OpenConfig),
            ("Run Task", CommandAction::RunTask),
            ("Switch Theme", CommandAction::SwitchTheme),
            ("Saved Layouts", CommandAction::ManageSavedLayouts),
            ("Minimize Window", CommandAction::MinimizeWindow),
            ("Kill All Tasks", CommandAction::KillAllTasks),
            ("Quit", CommandAction::Quit),
        ],
    },
];

/// 修饰键：（系统写法，显示写法）。
const MODIFIER_NAMES: &[(&str, &str)] = &[
    ("ctrl", "Ctrl"),
    ("alt", "Alt"),
    ("shift", "Shift"),
    ("cmd", "Win"),
    ("win", "Win"),
    ("super", "Win"),
    ("fn", "Fn"),
];

/// 把按键名转成显示文字，例如 `escape` → `Esc`，`left` → `←`，`d` → `D`。
fn display_key_name(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "escape" => "Esc".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        "delete" => "Del".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

/// 解析一组同时按下的键，例如 `ctrl-shift-d` → `["Ctrl", "Shift", "D"]`。
/// 按键本身可以是 `-`、`+`、`,` 等符号（如 `ctrl--`），所以只从头剥修饰键，剩下的整体当按键。
fn parse_chord(chord: &str) -> Vec<String> {
    let mut rest = chord;
    let mut keys = Vec::new();
    'strip: loop {
        for (raw, shown) in MODIFIER_NAMES {
            if let Some(tail) = rest
                .strip_prefix(raw)
                .and_then(|tail| tail.strip_prefix('-'))
                && !tail.is_empty()
            {
                keys.push((*shown).to_string());
                rest = tail;
                continue 'strip;
            }
        }
        break;
    }
    keys.push(display_key_name(rest));
    keys
}

/// 把快捷键文字拆成若干组按键：空格分隔的是先后按下的多组，每组内是同时按下的键。
fn shortcut_chords(label: &str) -> Vec<Vec<String>> {
    label
        .split_whitespace()
        .map(parse_chord)
        .filter(|chord| !chord.is_empty())
        .collect()
}

impl TerminalView {
    /// 快捷键弹窗是否打开。
    pub(super) fn shortcuts_popup_open(&self) -> bool {
        self.shortcuts_popup_open
    }

    /// 打开快捷键弹窗，并让列表回到顶部。
    pub(super) fn open_shortcuts_popup(&mut self, cx: &mut Context<Self>) {
        if self.shortcuts_popup_open {
            return;
        }
        self.shortcuts_popup_open = true;
        self.shortcuts_scroll = gpui_kit::ScrollHandle::new();
        self.notify_overlay(cx);
        cx.notify();
    }

    /// 关闭快捷键弹窗。
    pub(super) fn close_shortcuts_popup(&mut self, cx: &mut Context<Self>) {
        if self.shortcuts_popup_open {
            self.shortcuts_popup_open = false;
            self.notify_overlay(cx);
            cx.notify();
        }
    }

    /// 取出各分组里当前有绑定的条目：（分组标题，[(用途, 命令, 快捷键)]），空分组不返回。
    fn shortcut_rows(&self, window: &Window) -> Vec<ShortcutSectionRows> {
        SHORTCUT_SECTIONS
            .iter()
            .filter_map(|section| {
                let rows: Vec<_> = section
                    .items
                    .iter()
                    .filter_map(|(purpose, action)| {
                        action
                            .keybinding_label(window, &self.focus_handle)
                            .map(|label| (*purpose, *action, label))
                    })
                    .collect();
                (!rows.is_empty()).then_some((section.title, rows))
            })
            .collect()
    }

    /// 点击快捷键条目：先关弹窗，再执行命令，效果等同于按下对应快捷键。
    fn run_shortcut_command(
        &mut self,
        action: CommandAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_shortcuts_popup(cx);
        self.execute_command_action(action, false, window, cx);
    }

    /// 渲染快捷键弹窗；未打开时返回 `None`。
    pub(super) fn render_shortcuts_popup(
        &mut self,
        window: &mut Window,
        colors: &TerminalColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.shortcuts_popup_open {
            return None;
        }
        let overlay_style = self.overlay_style();
        let panel_bg = overlay_style.chrome_panel_background_with_floor(
            COMMAND_PALETTE_PANEL_BG_ALPHA,
            COMMAND_PALETTE_PANEL_SOLID_ALPHA,
        );
        let border = overlay_style.chrome_panel_neutral(0.16);
        let primary_text = overlay_style.panel_foreground(OVERLAY_PRIMARY_TEXT_ALPHA);
        let muted_text = overlay_style.panel_foreground(OVERLAY_MUTED_TEXT_ALPHA);
        let mut key_bg = colors.foreground;
        key_bg.a = 0.10;
        let mut row_hover = colors.foreground;
        row_hover.a = 0.05;
        let mut close_hover = colors.foreground;
        close_hover.a = 0.10;
        let mut scrim = colors.background;
        scrim.a = RELEASE_NOTES_SCRIM_ALPHA;

        let viewport = window.viewport_size();
        let viewport_width: f32 = viewport.width.into();
        let viewport_height: f32 = viewport.height.into();
        let panel_width = SHORTCUTS_PANEL_WIDTH.min((viewport_width - 48.0).max(320.0));
        let panel_max_height = SHORTCUTS_PANEL_MAX_HEIGHT
            .min((viewport_height - self.terminal_content_top_inset() - 48.0).max(240.0));

        // 列表：分组标题 + 每行“左用途、右快捷键”。
        let mut list = div().w_full().min_w(px(0.0)).flex().flex_col();
        for (section_index, (title, rows)) in self.shortcut_rows(window).into_iter().enumerate() {
            list = list.child(
                div()
                    .w_full()
                    .pt(px(if section_index == 0 { 0.0 } else { 14.0 }))
                    .pb(px(6.0))
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted_text)
                    .child(termy::i18n::tr(title)),
            );
            for (purpose, action, label) in rows {
                let mut chords = div().flex_none().flex().items_center().gap(px(8.0));
                for chord in shortcut_chords(&label) {
                    let mut keys = div().flex().items_center().gap(px(4.0));
                    for key in chord {
                        keys = keys.child(
                            div()
                                .h(px(20.0))
                                .min_w(px(20.0))
                                .px(px(6.0))
                                .rounded(px(4.0))
                                .bg(key_bg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(12.0))
                                .text_color(primary_text)
                                .child(key),
                        );
                    }
                    chords = chords.child(keys);
                }
                list = list.child(
                    div()
                        .id(SharedString::from(format!("shortcut-row-{action:?}")))
                        .w_full()
                        .h(px(32.0))
                        .px(px(8.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(16.0))
                        .cursor_pointer()
                        .hover(move |style| style.bg(row_hover))
                        // 点击条目等同于按下快捷键。
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                                this.run_shortcut_command(action, window, cx);
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .min_w(px(0.0))
                                .text_size(px(13.0))
                                .text_color(primary_text)
                                .child(termy::i18n::tr(purpose)),
                        )
                        .child(chords),
                );
            }
        }

        let panel = div()
            .id("shortcuts-panel")
            .w(px(panel_width))
            .max_h(px(panel_max_height))
            .rounded(px(14.0))
            .bg(panel_bg)
            .border_1()
            .border_color(border)
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _event, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .w_full()
                    .px(px(16.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(14.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(primary_text)
                                    .child(t!("Keyboard Shortcuts")),
                            )
                            .child(div().text_size(px(12.0)).text_color(muted_text).child(t!(
                                "Click an item to run it · Press Esc or click outside to close"
                            ))),
                    )
                    .child(
                        div()
                            .id("shortcuts-close")
                            .w(px(24.0))
                            .h(px(24.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(move |style| style.bg(close_hover))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.close_shortcuts_popup(cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(
                                gpui_kit::svg()
                                    .path(gpui_kit::SharedString::from("icons/tab_strip/x.svg"))
                                    .size(px(12.0))
                                    .text_color(muted_text),
                            ),
                    ),
            )
            .child(
                div()
                    .id("shortcuts-body")
                    .w_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .overflow_x_hidden()
                    .track_scroll(&self.shortcuts_scroll)
                    .px(px(10.0))
                    .py(px(12.0))
                    .child(list),
            );

        Some(
            div()
                .id("shortcuts-modal")
                .size_full()
                .absolute()
                .top_0()
                .left_0()
                .occlude()
                .bg(scrim)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        this.close_shortcuts_popup(cx);
                        cx.stop_propagation();
                    }),
                )
                .child(crate::ui::motion::enter_from_above(
                    panel,
                    "shortcuts-dialog-enter",
                ))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn parse_chord_splits_modifiers_and_key() {
        assert_eq!(parse_chord("ctrl-shift-d"), chord(&["Ctrl", "Shift", "D"]));
        assert_eq!(parse_chord("ctrl-t"), chord(&["Ctrl", "T"]));
    }

    #[test]
    fn parse_chord_keeps_symbol_keys_after_modifiers() {
        // 按键本身是 `-`、`+`、`,` 时不能被当成分隔符吞掉。
        assert_eq!(parse_chord("ctrl--"), chord(&["Ctrl", "-"]));
        assert_eq!(parse_chord("ctrl-+"), chord(&["Ctrl", "+"]));
        assert_eq!(parse_chord("ctrl-,"), chord(&["Ctrl", ","]));
    }

    #[test]
    fn parse_chord_uses_readable_names_for_special_keys() {
        assert_eq!(parse_chord("alt-left"), chord(&["Alt", "←"]));
        assert_eq!(parse_chord("escape"), chord(&["Esc"]));
        assert_eq!(parse_chord("ctrl-pageup"), chord(&["Ctrl", "PgUp"]));
        assert_eq!(parse_chord("ctrl-enter"), chord(&["Ctrl", "Enter"]));
    }

    #[test]
    fn parse_chord_maps_platform_modifier_to_win() {
        assert_eq!(parse_chord("win-d"), chord(&["Win", "D"]));
        assert_eq!(parse_chord("super-d"), chord(&["Win", "D"]));
    }

    #[test]
    fn shortcut_chords_splits_multi_stroke_sequences() {
        assert_eq!(
            shortcut_chords("ctrl-k ctrl-c"),
            vec![chord(&["Ctrl", "K"]), chord(&["Ctrl", "C"])]
        );
        assert!(shortcut_chords("   ").is_empty());
    }

    #[test]
    fn every_listed_command_appears_only_once() {
        let mut seen = std::collections::HashSet::new();
        for section in SHORTCUT_SECTIONS {
            for (_, action) in section.items {
                assert!(seen.insert(*action), "重复条目: {action:?}");
            }
        }
    }
}

//! 标签自定义指令：在标签右键菜单里添加「名字 + 指令」，
//! 该标签下所有窗格的右上角菜单都会多出名字项，点击即在该窗格里执行指令。

use super::*;
use gpui_kit::prelude::FluentBuilder as _;
use termy_core::session_model::StoredTabCommand;

/// 自定义指令名字的最大字符数。
const MAX_COMMAND_NAME_CHARS: usize = 24;
/// 对话框宽度（像素）。
const DIALOG_WIDTH: f32 = 520.0;
/// 对话框里输入框的高度（像素）。
const DIALOG_INPUT_HEIGHT: f32 = 26.0;
/// 对话框里名字输入框的宽度（像素）。
const DIALOG_NAME_WIDTH: f32 = 130.0;

/// 指令模板里由窗格自动提供的内置变量名。
const BUILTIN_VARS: [&str; 3] = ["cwd", "branch", "title"];

/// 指令模板展开结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Expansion {
    /// 展开完成，可以直接执行。
    Ready(String),
    /// 还缺这些窗格参数（按首次出现顺序、已去重），需要先问用户。
    NeedParams(Vec<String>),
    /// 内置变量此刻取不到值（例如窗格还没上报目录）。
    Unresolved(String),
}

/// 变量编辑窗口的状态：每行一对输入框（占位符名、值），同一时刻只有一个格子拿着输入焦点。
#[derive(Clone, Debug)]
pub(super) struct VarEditor {
    /// 正在编辑哪个窗格的变量。
    pub(super) pane_id: String,
    /// 所有行；至少保留一行。
    pub(super) rows: Vec<[InlineInputState; 2]>,
    /// 当前聚焦的行。
    pub(super) row: usize,
    /// 当前聚焦的列：0 = 名字，1 = 值。
    pub(super) col: usize,
}

impl VarEditor {
    /// 当前聚焦格子的输入状态。
    pub(super) fn cell(&self) -> &InlineInputState {
        &self.rows[self.row][self.col]
    }

    /// 当前聚焦格子的输入状态（可写）。
    pub(super) fn cell_mut(&mut self) -> &mut InlineInputState {
        &mut self.rows[self.row][self.col]
    }
}

/// 变量行校验失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum VarRowError {
    /// 变量名为空、含非法字符，或占用了内置变量名。
    InvalidName(String),
    /// 变量名有了但值为空。
    EmptyValue(String),
}

/// 把编辑窗口的各行（名字、值）规整成变量表。
///
/// - 名字两侧的空白和花括号会被去掉（填 `{port}` 等同 `port`）。
/// - 名字、值都空的行直接忽略；同名后写的覆盖先写的。
/// - 名字只允许字母数字下划线，且不能是内置变量名。
pub(super) fn normalize_var_rows(
    rows: &[(String, String)],
) -> Result<std::collections::BTreeMap<String, String>, VarRowError> {
    let mut out = std::collections::BTreeMap::new();
    for (raw_name, raw_value) in rows {
        let name = raw_name
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .trim();
        let value = raw_value.trim();
        if name.is_empty() && value.is_empty() {
            continue;
        }
        if name.is_empty()
            || BUILTIN_VARS.contains(&name)
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Err(VarRowError::InvalidName(name.to_string()));
        }
        if value.is_empty() {
            return Err(VarRowError::EmptyValue(name.to_string()));
        }
        out.insert(name.to_string(), value.to_string());
    }
    Ok(out)
}

/// 找出模板里的 `{name}` 占位符（name 仅限字母数字下划线；`${name}` 视为 shell 语法，跳过）。
///
/// 返回 (起始字节, 结束字节, 名字) 列表。
fn find_placeholders(template: &str) -> Vec<(usize, usize, &str)> {
    let bytes = template.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{'
            && (index == 0 || bytes[index - 1] != b'$')
            && let Some(len) = template[index + 1..].find('}')
        {
            let name = &template[index + 1..index + 1 + len];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                let end = index + len + 2;
                found.push((index, end, name));
                index = end;
                continue;
            }
        }
        index += 1;
    }
    found
}

/// 展开指令模板：内置变量（`cwd`/`branch`/`title`）取窗格实时值，其余 `{name}` 取窗格已保存的参数。
///
/// - `template`：用户写的指令，如 `npm run serve -- --port {port}`。
/// - `params`：该窗格已保存的参数。
/// - `builtin`：按名字取内置变量值，取不到返回 `None`。
pub(super) fn expand_command_template(
    template: &str,
    params: &std::collections::BTreeMap<String, String>,
    builtin: impl Fn(&str) -> Option<String>,
) -> Expansion {
    let mut out = String::new();
    let mut last = 0;
    let mut missing: Vec<String> = Vec::new();
    for (start, end, name) in find_placeholders(template) {
        out.push_str(&template[last..start]);
        last = end;
        if BUILTIN_VARS.contains(&name) {
            match builtin(name) {
                Some(value) => out.push_str(&value),
                None => return Expansion::Unresolved(name.to_string()),
            }
        } else if let Some(value) = params.get(name) {
            out.push_str(value);
        } else if !missing.iter().any(|item| item == name) {
            missing.push(name.to_string());
        }
    }
    if !missing.is_empty() {
        return Expansion::NeedParams(missing);
    }
    out.push_str(&template[last..]);
    Expansion::Ready(out)
}

/// 对话框里当前获得焦点的输入框。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CustomCommandField {
    /// 名字输入框。
    Name,
    /// 指令输入框。
    Command,
}

/// “新增自定义指令”对话框的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CustomCommandEdit {
    /// 指令要加到哪个标签。
    pub(super) tab_id: TabId,
    /// 当前聚焦的输入框。
    pub(super) field: CustomCommandField,
}

/// 把输入框里的原始文本规整成一条指令；名字或指令为空（去空白后）返回 `None`。
///
/// 名字超出 [`MAX_COMMAND_NAME_CHARS`] 会被截断；指令里的换行会被拍成空格，
/// 避免一条指令被拆成多次回车执行。
pub(super) fn normalize_tab_command(name: &str, command: &str) -> Option<StoredTabCommand> {
    let name: String = name.trim().chars().take(MAX_COMMAND_NAME_CHARS).collect();
    let command = command
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() || command.is_empty() {
        return None;
    }
    Some(StoredTabCommand { name, command })
}

/// 估算一段菜单文字的渲染宽度（像素，12px 字号）：ASCII 约 7px，其余按全角 12.5px 算。
///
/// - `text`：菜单项文字。
pub(super) fn estimate_menu_text_width(text: &str) -> f32 {
    text.chars()
        .map(|ch| if ch.is_ascii() { 7.0 } else { 12.5 })
        .sum()
}

impl TerminalView {
    /// 某个窗格所属标签的自定义指令；窗格不存在或标签没有指令时返回空列表。
    ///
    /// - `pane_id`：窗格 id。
    pub(super) fn tab_commands_for_pane(&self, pane_id: &str) -> Vec<StoredTabCommand> {
        self.session
            .tabs
            .iter()
            .find(|tab| tab.panes.iter().any(|pane| pane.id == pane_id))
            .and_then(|tab| self.tab_commands.get(&tab.id))
            .cloned()
            .unwrap_or_default()
    }

    /// 在指定窗格里执行所属标签的第 `index` 条自定义指令：直接把指令敲进该窗格并回车，
    /// 窗格的 shell 本来就在它自己的当前目录里。
    ///
    /// - `pane_id`：目标窗格 id。
    /// - `index`：指令在标签指令列表里的序号。
    pub(super) fn run_tab_command_in_pane(
        &mut self,
        pane_id: &str,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = self.tab_commands_for_pane(pane_id).get(index).cloned() else {
            return;
        };
        let params = self
            .pane_meta
            .get(pane_id)
            .map(|meta| meta.params.clone())
            .unwrap_or_default();
        let expansion = expand_command_template(&command.command, &params, |name| match name {
            "cwd" => self.pane_cwd(pane_id).map(str::to_owned),
            "branch" => self
                .pane_cwd(pane_id)
                .and_then(|cwd| self.branch_for_pane_cwd(cwd))
                .map(str::to_owned),
            "title" => Self::resolve_pane_title(&self.pane_meta, pane_id).map(str::to_owned),
            _ => None,
        });
        match expansion {
            Expansion::Ready(text) => {
                let input = format!("{text}\r");
                self.write_terminal_input_to_pane(pane_id, input.as_bytes(), cx);
            }
            Expansion::NeedParams(missing) => {
                crate::ui::toast::info(t!(
                    "Variable {name} is not set; use Edit Variables in the pane menu",
                    name = missing.join(", ")
                ));
                self.notify_overlay(cx);
            }
            Expansion::Unresolved(name) => {
                crate::ui::toast::info(t!("Cannot resolve {name} for this pane", name = name));
                self.notify_overlay(cx);
            }
        }
    }

    /// 打开“变量编辑”窗口：每行一对（占位符名、值），预填该窗格已保存的变量。
    ///
    /// - `pane_id`：要编辑变量的窗格。
    pub(super) fn begin_edit_pane_vars(&mut self, pane_id: &str, cx: &mut Context<Self>) {
        if self.is_command_palette_open() {
            self.close_command_palette(cx);
        }
        if self.search_open {
            self.close_search(cx);
        }
        self.cancel_rename_tab(cx);
        self.cancel_rename_workspace(cx);
        let _ = self.cancel_add_tab_command(cx);
        let mut rows: Vec<[InlineInputState; 2]> = self
            .pane_meta
            .get(pane_id)
            .map(|meta| {
                meta.params
                    .iter()
                    .map(|(name, value)| {
                        [
                            InlineInputState::new(name.clone()),
                            InlineInputState::new(value.clone()),
                        ]
                    })
                    .collect()
            })
            .unwrap_or_default();
        if rows.is_empty() {
            rows.push([
                InlineInputState::new(String::new()),
                InlineInputState::new(String::new()),
            ]);
        }
        self.var_editor = Some(VarEditor {
            pane_id: pane_id.to_owned(),
            rows,
            row: 0,
            col: 0,
        });
        self.inline_input_selecting = false;
        self.reset_cursor_blink_phase();
        self.notify_overlay(cx);
        cx.notify();
    }

    /// 关闭变量编辑窗口（不保存）；返回此前是否开着。
    pub(super) fn cancel_var_editor(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.var_editor.take().is_some();
        if was_open {
            self.inline_input_selecting = false;
            self.notify_overlay(cx);
            cx.notify();
        }
        was_open
    }

    /// 把输入焦点移到指定格子。
    ///
    /// - `row`：行号；超出范围时忽略。
    /// - `col`：0 = 名字，1 = 值。
    pub(super) fn focus_var_cell(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        let Some(editor) = self.var_editor.as_mut() else {
            return;
        };
        if row >= editor.rows.len() || col > 1 || (editor.row == row && editor.col == col) {
            return;
        }
        editor.row = row;
        editor.col = col;
        self.reset_cursor_blink_phase();
        self.notify_overlay(cx);
        cx.notify();
    }

    /// 末尾追加一个空行并聚焦它的名字格。
    pub(super) fn add_var_row(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.var_editor.as_mut() else {
            return;
        };
        editor.rows.push([
            InlineInputState::new(String::new()),
            InlineInputState::new(String::new()),
        ]);
        editor.row = editor.rows.len() - 1;
        editor.col = 0;
        self.reset_cursor_blink_phase();
        self.notify_overlay(cx);
        cx.notify();
    }

    /// 删除一行；只剩最后一行时改为清空它。
    ///
    /// - `index`：行号。
    pub(super) fn remove_var_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(editor) = self.var_editor.as_mut() else {
            return;
        };
        if index >= editor.rows.len() {
            return;
        }
        if editor.rows.len() == 1 {
            editor.rows[0] = [
                InlineInputState::new(String::new()),
                InlineInputState::new(String::new()),
            ];
        } else {
            editor.rows.remove(index);
            if index < editor.row {
                editor.row -= 1;
            }
        }
        editor.row = editor.row.min(editor.rows.len() - 1);
        self.reset_cursor_blink_phase();
        self.notify_overlay(cx);
        cx.notify();
    }

    /// 保存窗口里的全部变量（整体替换该窗格原有的变量）并关闭；校验失败时提示并保持打开。
    pub(super) fn commit_var_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.var_editor.as_ref() else {
            return;
        };
        let rows: Vec<(String, String)> = editor
            .rows
            .iter()
            .map(|row| (row[0].text().to_string(), row[1].text().to_string()))
            .collect();
        let pane_id = editor.pane_id.clone();
        match normalize_var_rows(&rows) {
            Ok(params) => {
                if params.is_empty() {
                    if let Some(meta) = self.pane_meta.get_mut(&pane_id) {
                        meta.params.clear();
                        if meta.is_empty() {
                            self.pane_meta.remove(&pane_id);
                        }
                    }
                } else {
                    self.pane_meta.entry(pane_id).or_default().params = params;
                }
                self.schedule_persist_native_workspace(cx);
                let _ = self.cancel_var_editor(cx);
            }
            Err(VarRowError::InvalidName(name)) => {
                crate::ui::toast::error(t!("Invalid variable name: {name}", name = name));
                self.notify_overlay(cx);
            }
            Err(VarRowError::EmptyValue(name)) => {
                crate::ui::toast::error(t!("Variable {name} needs a value", name = name));
                self.notify_overlay(cx);
            }
        }
    }

    /// 变量编辑窗口的按键：Enter 保存、Esc 取消、Tab 下一格（末格再按 Tab 新增一行）、上下键换行。
    ///
    /// - `key`：按键名（小写）。返回是否消费了该键。
    pub(super) fn handle_var_editor_key_down(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some((row, col, rows)) = self
            .var_editor
            .as_ref()
            .map(|editor| (editor.row, editor.col, editor.rows.len()))
        else {
            return false;
        };
        match key {
            "enter" => self.commit_var_editor(cx),
            "escape" => {
                let _ = self.cancel_var_editor(cx);
            }
            "tab" => {
                if col == 0 {
                    self.focus_var_cell(row, 1, cx);
                } else if row + 1 < rows {
                    self.focus_var_cell(row + 1, 0, cx);
                } else {
                    self.add_var_row(cx);
                }
            }
            "up" if row > 0 => self.focus_var_cell(row - 1, col, cx),
            "down" if row + 1 < rows => self.focus_var_cell(row + 1, col, cx),
            _ => return false,
        }
        true
    }

    /// 渲染“变量编辑”窗口：每行两个输入框（占位符名、值）加删除钮，底部有“添加一行”和“保存”。
    pub(super) fn render_var_editor_dialog(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.var_editor.as_ref()?;
        let focus = (editor.row, editor.col);
        let texts: Vec<[String; 2]> = editor
            .rows
            .iter()
            .map(|row| [row[0].text().to_string(), row[1].text().to_string()])
            .collect();
        let overlay_style = self.overlay_style();
        let panel_bg = overlay_style.chrome_panel_background(0.98);
        let panel_border = overlay_style.chrome_panel_neutral(0.22);
        let text_strong = overlay_style.panel_foreground(0.95);
        let text_muted = overlay_style.panel_foreground(0.5);
        let input_bg = overlay_style.panel_foreground(0.06);
        let active_border = overlay_style.chrome_panel_cursor(0.9);
        let hover_bg = overlay_style.chrome_panel_cursor(0.22);
        let selection = overlay_style.chrome_panel_cursor(0.35);
        let font = Font {
            family: self.ui_font_family.clone(),
            ..gpui_kit::font("")
        };

        let mut rows_el = Vec::<AnyElement>::new();
        for (row, pair) in texts.into_iter().enumerate() {
            let mut cells = Vec::<AnyElement>::new();
            for (col, text) in pair.into_iter().enumerate() {
                let focused = focus == (row, col);
                let empty = text.is_empty();
                let placeholder: SharedString = if col == 0 {
                    t!("Name (e.g. port)").into()
                } else {
                    t!("Value (e.g. 8080)").into()
                };
                let inner: AnyElement = if focused {
                    div()
                        .relative()
                        .size_full()
                        .child(self.render_inline_input_layer(
                            font.clone(),
                            px(12.0),
                            text_strong.into(),
                            selection.into(),
                            InlineInputAlignment::Left,
                            cx,
                        ))
                        .into_any_element()
                } else {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .text_size(px(12.0))
                        .text_color(text_strong)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(text)
                        .into_any_element()
                };
                let cell = div()
                    .id(("var-editor-cell", row * 2 + col))
                    .relative()
                    .h(px(DIALOG_INPUT_HEIGHT))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .bg(input_bg)
                    .border_1()
                    .border_color(if focused { active_border } else { panel_border })
                    .overflow_hidden()
                    .when(col == 0, |s| s.w(px(130.0)).flex_none())
                    .when(col == 1, |s| s.flex_1().min_w(px(0.0)))
                    .when(!focused, |s| {
                        s.cursor(gpui_kit::CursorStyle::IBeam).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _event: &MouseDownEvent, _window, cx| {
                                view.focus_var_cell(row, col, cx);
                                cx.stop_propagation();
                            }),
                        )
                    })
                    .when(empty, |s| {
                        s.child(
                            div()
                                .absolute()
                                .left(px(8.0))
                                .top_0()
                                .bottom_0()
                                .flex()
                                .items_center()
                                .text_size(px(12.0))
                                .text_color(text_muted)
                                .child(placeholder),
                        )
                    })
                    .child(inner)
                    .into_any_element();
                cells.push(cell);
            }
            rows_el.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .children(cells)
                    .child(
                        div()
                            .id(("var-editor-remove", row))
                            .flex_none()
                            .w(px(DIALOG_INPUT_HEIGHT))
                            .h(px(DIALOG_INPUT_HEIGHT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .text_size(px(13.0))
                            .text_color(text_muted)
                            .cursor_pointer()
                            .hover(|style| style.bg(hover_bg))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |view, _event: &MouseDownEvent, _window, cx| {
                                    view.remove_var_row(row, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child("×"),
                    )
                    .into_any_element(),
            );
        }

        let button = |id: &'static str, label: SharedString| {
            div()
                .id(id)
                .h(px(DIALOG_INPUT_HEIGHT))
                .px(px(12.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .border_1()
                .border_color(panel_border)
                .text_size(px(12.0))
                .text_color(text_strong)
                .cursor_pointer()
                .hover(|style| style.bg(hover_bg))
                .child(label)
        };

        Some(
            div()
                .id("var-editor-overlay")
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(90.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                        let _ = view.cancel_var_editor(cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .id("var-editor-panel")
                        .occlude()
                        .w(px(DIALOG_WIDTH))
                        .px(px(10.0))
                        .py(px(8.0))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .bg(panel_bg)
                        .border_1()
                        .border_color(panel_border)
                        .rounded(px(8.0))
                        .shadow_lg()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_view, _event: &MouseDownEvent, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(text_muted)
                                .child(t!("Edit Variables")),
                        )
                        .child(
                            div()
                                .id("var-editor-rows")
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .max_h(px(240.0))
                                .overflow_y_scroll()
                                .children(rows_el),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    button("var-editor-add", t!("Add Row").into()).on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(
                                            |view, _event: &MouseDownEvent, _window, cx| {
                                                view.add_var_row(cx);
                                                cx.stop_propagation();
                                            },
                                        ),
                                    ),
                                )
                                .child(button("var-editor-save", t!("Save").into()).on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                                        view.commit_var_editor(cx);
                                        cx.stop_propagation();
                                    }),
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(px(10.5))
                                        .text_color(text_muted)
                                        .child(format!(
                                            "{} · {{cwd}} {{branch}} {{title}}",
                                            t!("Tab next · Enter save · Esc cancel")
                                        )),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 打开“新增自定义指令”对话框，先关掉其它会抢输入的浮层。
    ///
    /// - `tab_id`：指令要加到的标签。
    pub(super) fn begin_add_tab_command(&mut self, tab_id: TabId, cx: &mut Context<Self>) {
        if self.is_command_palette_open() {
            self.close_command_palette(cx);
        }
        if self.search_open {
            self.close_search(cx);
        }
        self.cancel_rename_tab(cx);
        self.cancel_rename_workspace(cx);
        let _ = self.close_terminal_context_menu(cx);
        let _ = self.close_tab_context_menu(cx);
        self.custom_command_name_input.clear();
        self.custom_command_text_input.clear();
        self.custom_command_edit = Some(CustomCommandEdit {
            tab_id,
            field: CustomCommandField::Name,
        });
        self.inline_input_selecting = false;
        self.reset_cursor_blink_phase();
        self.notify_overlay(cx);
        cx.notify();
    }

    /// 关闭对话框并清空两个输入框；返回对话框此前是否开着。
    pub(super) fn cancel_add_tab_command(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.custom_command_edit.take().is_some();
        let had_text = !self.custom_command_name_input.text().is_empty()
            || !self.custom_command_text_input.text().is_empty();
        self.custom_command_name_input.clear();
        self.custom_command_text_input.clear();
        self.inline_input_selecting = false;
        if was_open || had_text {
            self.reset_cursor_blink_phase();
            self.notify_overlay(cx);
            cx.notify();
        }
        was_open
    }

    /// 切换到另一个输入框（Tab 键或点击输入框）。
    ///
    /// - `field`：要聚焦的输入框。
    pub(super) fn focus_custom_command_field(
        &mut self,
        field: CustomCommandField,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.custom_command_edit.as_mut() else {
            return;
        };
        if edit.field == field {
            return;
        }
        edit.field = field;
        self.reset_cursor_blink_phase();
        self.notify_overlay(cx);
        cx.notify();
    }

    /// 回车：两个输入框都填好就保存；否则跳到还没填的那个。
    pub(super) fn commit_add_tab_command(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.custom_command_edit else {
            return;
        };
        let name = self.custom_command_name_input.text().to_string();
        let command = self.custom_command_text_input.text().to_string();
        let Some(entry) = normalize_tab_command(&name, &command) else {
            let missing = if name.trim().is_empty() {
                CustomCommandField::Name
            } else {
                CustomCommandField::Command
            };
            self.focus_custom_command_field(missing, cx);
            return;
        };
        if self.tab_index_by_id(edit.tab_id).is_some() {
            self.tab_commands
                .entry(edit.tab_id)
                .or_default()
                .push(entry);
            self.schedule_persist_native_workspace(cx);
        }
        let _ = self.cancel_add_tab_command(cx);
    }

    /// 删除标签的第 `index` 条自定义指令。
    ///
    /// - `tab_id`：标签 id。
    /// - `index`：指令序号。
    pub(super) fn remove_tab_command(
        &mut self,
        tab_id: TabId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(commands) = self.tab_commands.get_mut(&tab_id) else {
            return;
        };
        if index >= commands.len() {
            return;
        }
        commands.remove(index);
        if commands.is_empty() {
            self.tab_commands.remove(&tab_id);
        }
        self.schedule_persist_native_workspace(cx);
        cx.notify();
    }

    /// 对话框里按键的处理：Enter 保存、Esc 取消、Tab 切换输入框。返回是否已消费该键。
    ///
    /// - `key`：按键名（小写）。
    pub(super) fn handle_custom_command_key_down(
        &mut self,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(edit) = self.custom_command_edit else {
            return false;
        };
        match key {
            "enter" => self.commit_add_tab_command(cx),
            "escape" => {
                let _ = self.cancel_add_tab_command(cx);
            }
            "tab" => {
                let next = match edit.field {
                    CustomCommandField::Name => CustomCommandField::Command,
                    CustomCommandField::Command => CustomCommandField::Name,
                };
                self.focus_custom_command_field(next, cx);
            }
            _ => return false,
        }
        true
    }

    /// 渲染“新增自定义指令”对话框：一行两个输入框（名字、指令）加一个确定按钮。
    pub(super) fn render_custom_command_dialog(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let edit = self.custom_command_edit?;
        let overlay_style = self.overlay_style();
        let panel_bg = overlay_style.chrome_panel_background(0.98);
        let panel_border = overlay_style.chrome_panel_neutral(0.22);
        let text_strong = overlay_style.panel_foreground(0.95);
        let text_muted = overlay_style.panel_foreground(0.5);
        let input_bg = overlay_style.panel_foreground(0.06);
        let active_border = overlay_style.chrome_panel_cursor(0.9);
        let hover_bg = overlay_style.chrome_panel_cursor(0.22);
        let selection = overlay_style.chrome_panel_cursor(0.35);
        let font = Font {
            family: self.ui_font_family.clone(),
            ..gpui_kit::font("")
        };

        // 单个输入框：聚焦的那个挂真正的输入层，另一个只显示文字，点击即切换。
        let mut field_box = |id: &'static str,
                             field: CustomCommandField,
                             placeholder: SharedString,
                             text: String| {
            let focused = edit.field == field;
            let empty = text.is_empty();
            let inner: AnyElement = if focused {
                div()
                    .relative()
                    .size_full()
                    .child(self.render_inline_input_layer(
                        font.clone(),
                        px(12.0),
                        text_strong.into(),
                        selection.into(),
                        InlineInputAlignment::Left,
                        cx,
                    ))
                    .into_any_element()
            } else {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .text_color(text_strong)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(text)
                    .into_any_element()
            };
            div()
                .id(id)
                .relative()
                .h(px(DIALOG_INPUT_HEIGHT))
                .px(px(8.0))
                .rounded(px(5.0))
                .bg(input_bg)
                .border_1()
                .border_color(if focused { active_border } else { panel_border })
                .overflow_hidden()
                .when(!focused, |s| {
                    s.cursor(gpui_kit::CursorStyle::IBeam).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _event: &MouseDownEvent, _window, cx| {
                            view.focus_custom_command_field(field, cx);
                            cx.stop_propagation();
                        }),
                    )
                })
                .when(empty, |s| {
                    s.child(
                        div()
                            .absolute()
                            .left(px(8.0))
                            .top_0()
                            .bottom_0()
                            .flex()
                            .items_center()
                            .text_size(px(12.0))
                            .text_color(text_muted)
                            .child(placeholder),
                    )
                })
                .child(inner)
        };

        let name_box = field_box(
            "custom-command-name",
            CustomCommandField::Name,
            t!("Name").into(),
            self.custom_command_name_input.text().to_string(),
        )
        .w(px(DIALOG_NAME_WIDTH))
        .flex_none();
        let command_box = field_box(
            "custom-command-text",
            CustomCommandField::Command,
            t!("Command").into(),
            self.custom_command_text_input.text().to_string(),
        )
        .flex_1()
        .min_w(px(0.0));

        Some(
            div()
                .id("custom-command-overlay")
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(90.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                        let _ = view.cancel_add_tab_command(cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .id("custom-command-panel")
                        .occlude()
                        .w(px(DIALOG_WIDTH))
                        .px(px(10.0))
                        .py(px(8.0))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .bg(panel_bg)
                        .border_1()
                        .border_color(panel_border)
                        .rounded(px(8.0))
                        .shadow_lg()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_view, _event: &MouseDownEvent, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(text_muted)
                                .child(t!("Add Custom Command")),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(name_box)
                                .child(command_box)
                                .child(
                                    div()
                                        .id("custom-command-confirm")
                                        .flex_none()
                                        .h(px(DIALOG_INPUT_HEIGHT))
                                        .px(px(12.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(5.0))
                                        .border_1()
                                        .border_color(panel_border)
                                        .text_size(px(12.0))
                                        .text_color(text_strong)
                                        .cursor_pointer()
                                        .hover(|style| style.bg(hover_bg))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                |view, _event: &MouseDownEvent, _window, cx| {
                                                    view.commit_add_tab_command(cx);
                                                    cx.stop_propagation();
                                                },
                                            ),
                                        )
                                        .child(t!("Add")),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(text_muted)
                                .child(t!("Tab to switch field · Enter to add · Esc to cancel")),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_requires_both_fields() {
        assert!(normalize_tab_command("  ", "ls").is_none());
        assert!(normalize_tab_command("ls", " \n ").is_none());
    }

    #[test]
    fn normalize_trims_and_flattens_newlines() {
        let entry = normalize_tab_command(" build ", " cargo build\n  --release \r\n").unwrap();
        assert_eq!(entry.name, "build");
        assert_eq!(entry.command, "cargo build --release");
    }

    #[test]
    fn normalize_truncates_long_names() {
        let entry = normalize_tab_command(&"a".repeat(100), "x").unwrap();
        assert_eq!(entry.name.chars().count(), MAX_COMMAND_NAME_CHARS);
    }

    fn builtin(name: &str) -> Option<String> {
        match name {
            "cwd" => Some("/work/api".to_string()),
            _ => None,
        }
    }

    fn params(entries: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn expand_uses_saved_params_and_builtins() {
        assert_eq!(
            expand_command_template(
                "npm run serve -- --port {port} # {cwd}",
                &params(&[("port", "8081")]),
                builtin
            ),
            Expansion::Ready("npm run serve -- --port 8081 # /work/api".to_string())
        );
    }

    #[test]
    fn expand_reports_missing_params_once_in_order() {
        assert_eq!(
            expand_command_template("run {b} {a} {b}", &params(&[]), builtin),
            Expansion::NeedParams(vec!["b".to_string(), "a".to_string()])
        );
    }

    #[test]
    fn expand_reports_unresolved_builtin() {
        assert_eq!(
            expand_command_template("echo {branch}", &params(&[]), builtin),
            Expansion::Unresolved("branch".to_string())
        );
    }

    #[test]
    fn expand_leaves_shell_and_json_braces_alone() {
        let text = r#"echo ${HOME} '{"a": 1}' {} { x }"#;
        assert_eq!(
            expand_command_template(text, &params(&[]), builtin),
            Expansion::Ready(text.to_string())
        );
    }

    fn rows(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn var_rows_skip_blank_and_strip_braces() {
        let out =
            normalize_var_rows(&rows(&[(" {port} ", " 8080 "), ("", " "), ("host", "a")])).unwrap();
        assert_eq!(out.get("port").map(String::as_str), Some("8080"));
        assert_eq!(out.get("host").map(String::as_str), Some("a"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn var_rows_later_duplicate_wins() {
        let out = normalize_var_rows(&rows(&[("p", "1"), ("p", "2")])).unwrap();
        assert_eq!(out.get("p").map(String::as_str), Some("2"));
    }

    #[test]
    fn var_rows_reject_bad_names_and_empty_values() {
        assert_eq!(
            normalize_var_rows(&rows(&[("", "1")])),
            Err(VarRowError::InvalidName(String::new()))
        );
        assert_eq!(
            normalize_var_rows(&rows(&[("a-b", "1")])),
            Err(VarRowError::InvalidName("a-b".to_string()))
        );
        assert_eq!(
            normalize_var_rows(&rows(&[("cwd", "1")])),
            Err(VarRowError::InvalidName("cwd".to_string()))
        );
        assert_eq!(
            normalize_var_rows(&rows(&[("port", " ")])),
            Err(VarRowError::EmptyValue("port".to_string()))
        );
    }

    #[test]
    fn menu_width_counts_cjk_wider() {
        assert!(estimate_menu_text_width("构建") > estimate_menu_text_width("ab"));
        assert_eq!(estimate_menu_text_width(""), 0.0);
    }
}

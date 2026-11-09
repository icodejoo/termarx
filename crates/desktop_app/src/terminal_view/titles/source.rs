use super::super::*;
use crate::terminal_ui::TmuxPaneState;
use std::path::Path;

/// Cygwin 的盘符挂载前缀（`/cygdrive/c/x`）。
const CYGDRIVE_PREFIX: &str = "/cygdrive";
/// 盘符后冒号的 URL 编码形式（OSC 7 里 `E:` 可能写成 `E%3A`）；裸冒号放最后兜底。
const DRIVE_COLON_PREFIXES: [&str; 3] = ["%3A", "%3a", ":"];
/// 盘符之后合法的路径分隔符起始形式（含 URL 编码的反斜杠）。
const DRIVE_SEPARATOR_PREFIXES: [&str; 4] = ["/", "\\", "%5C", "%5c"];
/// 路径里可能出现的两种分隔符。
const PATH_SEPARATORS: [char; 2] = ['/', '\\'];
/// Windows 路径分隔符。
const WINDOWS_SEPARATOR: &str = "\\";
/// POSIX 路径分隔符。
const POSIX_SEPARATOR: &str = "/";

/// 窗格标签的两段文字：左段用标题样式，右段（可选）用路径样式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaneLabelTexts {
    /// 左段：自定义标题；没有自定义标题时就是路径。
    pub(crate) left: String,
    /// 右段：路径；仅在有自定义标题且目录已知时存在。
    pub(crate) right: Option<String>,
}

/// 按窗格 id 存的侧表数据：手动名、终端上报标题、shell 上报的当前目录。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaneMeta {
    /// 用户手动起的窗格名；窗格标签里唯一算“自定义标题”的来源。
    pub(crate) manual_title: Option<String>,
    /// 终端上报的标题；只参与持久化，不用于窗格标签显示。
    pub(crate) reported_title: Option<String>,
    /// shell 上报的当前目录（OSC 7 / OSC 9;9）。
    pub(crate) cwd: Option<String>,
    /// 自定义指令的窗格级参数（占位符名 -> 值），例如 `port` -> `8080`。
    pub(crate) params: std::collections::BTreeMap<String, String>,
    /// 路径 -> 窗格名：手动名按路径保存，shell 切换目录时标题跟着换。
    pub(crate) path_titles: std::collections::BTreeMap<String, String>,
}

impl PaneMeta {
    /// 所有字段是否都为空（空条目会从侧表里移除）。
    pub(crate) fn is_empty(&self) -> bool {
        self.manual_title.is_none()
            && self.reported_title.is_none()
            && self.cwd.is_none()
            && self.params.is_empty()
            && self.path_titles.is_empty()
    }
}

impl TerminalView {
    pub(crate) fn fallback_title(&self) -> &str {
        let fallback = self.tab_title.fallback.trim();
        if fallback.is_empty() {
            DEFAULT_TAB_TITLE
        } else {
            fallback
        }
    }

    pub(crate) fn resolve_template(
        template: &str,
        cwd: Option<&str>,
        command: Option<&str>,
    ) -> String {
        const CWD_TOKEN: &str = "{cwd}";
        const COMMAND_TOKEN: &str = "{command}";

        let cwd = cwd.unwrap_or("");
        let command = command.unwrap_or("");
        let mut remaining = template;
        let mut resolved = String::with_capacity(template.len());

        while let Some(open_brace_idx) = remaining.find('{') {
            resolved.push_str(&remaining[..open_brace_idx]);
            remaining = &remaining[open_brace_idx..];
            if let Some(tail) = remaining.strip_prefix(CWD_TOKEN) {
                resolved.push_str(cwd);
                remaining = tail;
                continue;
            }
            if let Some(tail) = remaining.strip_prefix(COMMAND_TOKEN) {
                resolved.push_str(command);
                remaining = tail;
                continue;
            }

            resolved.push('{');
            remaining = &remaining['{'.len_utf8()..];
        }

        resolved.push_str(remaining);
        resolved
    }

    pub(crate) fn is_shell_command(command: &str) -> bool {
        let command = command.trim();
        if command.is_empty() {
            return false;
        }
        let Some(first_token) = command.split_whitespace().next() else {
            return false;
        };
        let token = Path::new(first_token)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(first_token)
            .trim_start_matches('-');
        if token.is_empty() {
            return false;
        }
        let mut normalized = token.to_ascii_lowercase();
        if let Some(stem) = normalized.strip_suffix(".exe") {
            normalized = stem.to_string();
        }

        matches!(
            normalized.as_str(),
            "bash"
                | "zsh"
                | "fish"
                | "sh"
                | "dash"
                | "ksh"
                | "tcsh"
                | "csh"
                | "nu"
                | "pwsh"
                | "powershell"
                | "cmd"
        )
    }

    fn display_cwd_for_tab_title(cwd: Option<&str>) -> Option<String> {
        let cwd = cwd.map(str::trim).filter(|cwd| !cwd.is_empty())?;
        Some(Self::display_working_directory_for_prompt(Path::new(cwd)))
    }

    fn resolve_prompt_title(template: &str, cwd: Option<&str>) -> String {
        let cwd = Self::display_cwd_for_tab_title(cwd);
        Self::resolve_template(template, cwd.as_deref(), None)
    }

    pub(crate) fn derive_tmux_shell_title(
        tab_title: &TabTitleConfig,
        pane: &TmuxPaneState,
    ) -> Option<String> {
        let cwd = pane.current_path.trim();
        let command = pane.current_command.trim();
        let resolved = if Self::is_shell_command(command) {
            Self::resolve_prompt_title(&tab_title.prompt_format, (!cwd.is_empty()).then_some(cwd))
        } else {
            Self::resolve_template(
                &tab_title.command_format,
                None,
                (!command.is_empty()).then_some(command),
            )
        };

        let resolved = resolved.trim();
        if resolved.is_empty() {
            return None;
        }

        Some(Self::truncate_tab_title(resolved))
    }

    pub(crate) fn should_seed_predicted_prompt_title(tab_title: &TabTitleConfig) -> bool {
        tab_title.priority.contains(&TabTitleSource::Explicit)
    }

    pub(crate) fn predicted_prompt_seed_title(
        tab_title: &TabTitleConfig,
        cwd: Option<&str>,
    ) -> Option<String> {
        if !Self::should_seed_predicted_prompt_title(tab_title) {
            return None;
        }

        let resolved = Self::resolve_prompt_title(&tab_title.prompt_format, cwd);
        let resolved = resolved.trim();
        if resolved.is_empty() {
            return None;
        }

        Some(Self::truncate_tab_title(resolved))
    }

    fn smart_mode_shell_fallback_enabled(tab_title: &TabTitleConfig) -> bool {
        tab_title.mode == termy_core::config_core::TabTitleMode::Smart
            && !tab_title.shell_integration
            && tab_title.priority.contains(&TabTitleSource::Explicit)
            && tab_title.priority.contains(&TabTitleSource::Shell)
    }

    /// Returns the title candidate for a single source in the priority list.
    ///
    /// The caller walks sources in priority order and uses the first non-empty
    /// candidate.  The `Explicit` source has two special deference modes that
    /// both yield to `shell_title` when available:
    ///
    /// - **Prediction**: the explicit title was pre-seeded at tab creation from
    ///   the cwd and has not yet been confirmed by a shell-integration event.
    /// - **Smart-mode shell fallback**: shell integration is disabled in smart
    ///   mode, so the shell's own title (set via terminal escape sequences) is
    ///   preferred once available.
    ///
    /// In both cases the explicit title is kept as a fallback so the tab is
    /// never blank.
    fn title_source_candidate<'a>(
        source: TabTitleSource,
        manual_title: Option<&'a str>,
        explicit_title: Option<&'a str>,
        explicit_title_is_prediction: bool,
        prediction_allows_shell: bool,
        shell_title: Option<&'a str>,
        fallback_title: &'a str,
        smart_mode_shell_fallback: bool,
    ) -> Option<&'a str> {
        match source {
            TabTitleSource::Manual => manual_title,
            // The explicit title is speculative—prefer a live shell title.
            TabTitleSource::Explicit if explicit_title_is_prediction && prediction_allows_shell => {
                shell_title.or(explicit_title)
            }
            TabTitleSource::Explicit if explicit_title_is_prediction => explicit_title,
            TabTitleSource::Explicit if smart_mode_shell_fallback => {
                // Smart mode seeds an explicit title before the shell emits a title.
                // When shell integration is disabled, prefer live shell titles once
                // available while keeping explicit as a fallback.
                shell_title.or(explicit_title)
            }
            TabTitleSource::Explicit => explicit_title,
            TabTitleSource::Shell => shell_title,
            TabTitleSource::Fallback => Some(fallback_title),
        }
    }

    fn parse_explicit_title(&self, title: &str) -> Option<ExplicitTitlePayload> {
        let prefix = self.tab_title.explicit_prefix.trim();
        if prefix.is_empty() {
            return None;
        }

        let payload = title.strip_prefix(prefix)?.trim();
        if payload.is_empty() {
            return None;
        }

        if let Some(prompt) = payload.strip_prefix("prompt:") {
            let prompt = prompt.trim();
            if prompt.is_empty() {
                return None;
            }
            return Some(ExplicitTitlePayload::Prompt {
                title: Self::resolve_prompt_title(&self.tab_title.prompt_format, Some(prompt)),
                cwd: prompt.to_string(),
            });
        }

        if let Some(command) = payload.strip_prefix("command:") {
            let command = command.trim();
            if command.is_empty() {
                return None;
            }
            return Some(ExplicitTitlePayload::Command {
                title: Self::resolve_template(&self.tab_title.command_format, None, Some(command)),
                command: command.to_string(),
            });
        }

        let explicit = payload.strip_prefix("title:").unwrap_or(payload).trim();
        if explicit.is_empty() {
            return None;
        }

        Some(ExplicitTitlePayload::Title(explicit.to_string()))
    }

    /// 把终端上报的原始标题转成窗格标签文字。
    /// 带内部前缀（`termy:tab:`）的载荷会按标签标题同样的规则解析，其余原样清洗。
    /// 清洗后为空返回 `None`。
    pub(crate) fn pane_display_title(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        let text = match self.parse_explicit_title(raw) {
            Some(
                ExplicitTitlePayload::Prompt { title, .. }
                | ExplicitTitlePayload::Command { title, .. }
                | ExplicitTitlePayload::Title(title),
            ) => title,
            None => raw.to_string(),
        };
        let text = Self::truncate_tab_title(text.trim());
        (!text.is_empty()).then_some(text)
    }

    /// 记录某个窗格的标题，返回显示内容是否发生变化（决定是否需要重绘）。
    pub(crate) fn record_pane_title(&mut self, pane_id: &str, raw: &str) -> bool {
        let Some(title) = self.pane_display_title(raw) else {
            return false;
        };
        let meta = self.pane_meta.entry(pane_id.to_string()).or_default();
        if meta.reported_title.as_ref() == Some(&title) {
            return false;
        }
        meta.reported_title = Some(title);
        true
    }

    /// 清除某个窗格记录的终端上报标题，返回是否真的有内容被清掉。
    pub(crate) fn clear_pane_reported_title(&mut self, pane_id: &str) -> bool {
        let Some(meta) = self.pane_meta.get_mut(pane_id) else {
            return false;
        };
        let cleared = meta.reported_title.take().is_some();
        if meta.is_empty() {
            self.pane_meta.remove(pane_id);
        }
        cleared
    }

    /// 某个窗格当前的目录（shell 上报），没记录返回 `None`。
    pub(crate) fn pane_cwd(&self, pane_id: &str) -> Option<&str> {
        self.pane_meta.get(pane_id)?.cwd.as_deref()
    }

    /// 把 shell 上报的 cwd 文本规范成真实目录路径：去首尾空白与引号；
    /// Windows 上还把 Git Bash / MSYS2 / Cygwin 的 `/c/...` 转回盘符路径。
    /// 不截断、不折叠空白（截断只用于渲染），也不解码 `%XX`（OSC 7 在解析时已解码）。
    ///
    /// - `raw`：终端上报的原始路径文本。
    ///
    /// 返回规范化后的路径；上报为空时返回空串。
    pub(crate) fn normalize_reported_cwd(raw: &str) -> String {
        let raw = raw.trim().trim_matches('"');
        if cfg!(target_os = "windows") {
            Self::normalize_drive_style_cwd(raw)
        } else {
            raw.to_string()
        }
    }

    /// 记录某个窗格的当前目录（来自 shell 上报的 OSC 7 / OSC 9;9），
    /// 返回显示内容是否发生变化。
    pub(crate) fn record_pane_cwd(&mut self, pane_id: &str, raw: &str) -> bool {
        let cwd = Self::normalize_reported_cwd(raw);
        Self::apply_pane_cwd(&mut self.pane_meta, pane_id, cwd)
    }

    /// 把已规范化的目录写入侧表，并让手动名跟着路径走，返回是否发生变化。
    ///
    /// - 首次拿到路径前就起了名：该名字绑定到这个路径。
    /// - 目录变了：标题切到新路径保存的名字，没有则清空（回落成显示路径）。
    ///
    /// - `metas`：窗格侧表（窗格 id -> 元数据）。
    /// - `pane_id`：目标窗格 id。
    /// - `cwd`：规范化后的目录，空串视为无变化。
    pub(crate) fn apply_pane_cwd(
        metas: &mut HashMap<String, PaneMeta>,
        pane_id: &str,
        cwd: String,
    ) -> bool {
        let current = metas.get(pane_id).and_then(|meta| meta.cwd.as_deref());
        if cwd.is_empty() || current == Some(cwd.as_str()) {
            return false;
        }
        let meta = metas.entry(pane_id.to_string()).or_default();
        let first_report = meta.cwd.is_none();
        meta.cwd = Some(cwd.clone());
        if first_report && let Some(title) = meta.manual_title.clone() {
            meta.path_titles.insert(cwd, title);
        } else {
            meta.manual_title = meta.path_titles.get(&cwd).cloned();
        }
        true
    }

    /// 解析窗格标签使用的标题：只有用户手动设置的名字才算自定义标题。
    ///
    /// 终端上报的标题（如 cmd 默认的 `C:\Windows\system32\cmd.exe`）不参与，
    /// 没有自定义标题时由调用方用当前路径当标题（见 `pane_label_texts`）。
    ///
    /// - `metas`：窗格侧表（窗格 id -> 元数据）。
    /// - `pane_id`：目标窗格 id。
    ///
    /// 返回应显示的自定义标题，没有时为 `None`。
    pub(crate) fn resolve_pane_title<'a>(
        metas: &'a HashMap<String, PaneMeta>,
        pane_id: &str,
    ) -> Option<&'a str> {
        metas.get(pane_id)?.manual_title.as_deref()
    }

    /// 把用户输入写入侧表的手动名：截断后为空则清除，返回是否发生变化。
    ///
    /// - `metas`：窗格侧表（窗格 id -> 元数据）。
    /// - `pane_id`：目标窗格 id。
    /// - `raw`：用户输入的原始文本。
    pub(crate) fn apply_manual_pane_title(
        metas: &mut HashMap<String, PaneMeta>,
        pane_id: &str,
        raw: &str,
    ) -> bool {
        let title = Self::truncate_tab_title(raw.trim());
        if title.is_empty() {
            let Some(meta) = metas.get_mut(pane_id) else {
                return false;
            };
            let cleared = meta.manual_title.take().is_some();
            if let Some(cwd) = meta.cwd.clone() {
                meta.path_titles.remove(&cwd);
            }
            if meta.is_empty() {
                metas.remove(pane_id);
            }
            return cleared;
        }
        let meta = metas.entry(pane_id.to_string()).or_default();
        if meta.manual_title.as_ref() == Some(&title) {
            return false;
        }
        if let Some(cwd) = meta.cwd.clone() {
            meta.path_titles.insert(cwd, title.clone());
        }
        meta.manual_title = Some(title);
        true
    }

    /// 设置（或清除，传空串）某个窗格的手动名，返回是否发生变化。
    pub(crate) fn set_pane_manual_title(&mut self, pane_id: &str, raw: &str) -> bool {
        Self::apply_manual_pane_title(&mut self.pane_meta, pane_id, raw)
    }

    /// 窗格被关闭后回收按窗格 id 存的侧表（手动名、上报标题、cwd），
    /// 再重新对齐 git 监听并刷新标签标题。
    ///
    /// 以“不在任何存活窗格里”为准，所以关闭窗格、标签、工作区后都可以直接调用；
    /// 不回收的话，复用 `%native-pane-N` 的新窗格会继承旧路径和分支后缀。
    pub(crate) fn prune_dead_pane_state(&mut self, cx: &mut Context<Self>) {
        let live = self.session.live_pane_ids();
        self.pane_meta.retain(|id, _| live.contains(id.as_str()));
        self.sync_git_watch(cx);
        for index in 0..self.session.tabs.len() {
            self.refresh_tab_title(index);
        }
    }

    /// 把盘符风格的 POSIX 路径转成 Windows 路径：Git Bash / MSYS2 / Cygwin 的 `/c/Users/x`、
    /// `/cygdrive/c/Users/x`，以及 OSC 7 的 `/E:/x`、`/E%3A/x`、`/E:\x`、`/E:%5Cx`，
    /// 都转成 `C:\Users\x` 形式；不是这种形式的路径原样返回。
    ///
    /// - `raw`：shell 上报的路径文本。
    ///
    /// 盘符后的 `%XX` 不在此处解码（OSC 7 在解析时已解码）。
    pub(crate) fn normalize_drive_style_cwd(raw: &str) -> String {
        let rest = raw.strip_prefix(CYGDRIVE_PREFIX).unwrap_or(raw);
        let Some(after) = rest.strip_prefix(POSIX_SEPARATOR) else {
            return raw.to_string();
        };
        let mut chars = after.chars();
        let Some(drive) = chars.next().filter(char::is_ascii_alphabetic) else {
            return raw.to_string();
        };
        let tail = chars.as_str();
        // 盘符后允许跟冒号（或编码的冒号），随后必须是结尾或路径分隔符。
        let tail = DRIVE_COLON_PREFIXES
            .iter()
            .find_map(|colon| tail.strip_prefix(colon))
            .unwrap_or(tail);
        if tail.is_empty() {
            return format!("{}:{WINDOWS_SEPARATOR}", drive.to_ascii_uppercase());
        }
        let has_separator = DRIVE_SEPARATOR_PREFIXES
            .iter()
            .any(|separator| tail.starts_with(separator));
        if !has_separator {
            return raw.to_string();
        }
        format!(
            "{}:{}",
            drive.to_ascii_uppercase(),
            tail.replace(POSIX_SEPARATOR, WINDOWS_SEPARATOR)
        )
    }

    /// 比较两段文字是否指向同一路径：忽略首尾空白、末尾分隔符、`/` 与 `\` 的差别和大小写。
    fn same_path_text(a: &str, b: &str) -> bool {
        let normalize = |text: &str| {
            text.trim()
                .trim_end_matches(PATH_SEPARATORS)
                .replace(WINDOWS_SEPARATOR, POSIX_SEPARATOR)
                .to_lowercase()
        };
        normalize(a) == normalize(b)
    }

    /// 计算窗格标签的两段文字。
    ///
    /// - 标题与当前目录不同 → 有自定义标题：左标题、右路径（目录未知时只有左标题）；
    /// - 否则（没有标题，或标题只是路径本身）→ 路径就是标题：只有左段。
    ///
    /// 标题和目录都没有时返回 `None`。
    pub(crate) fn pane_label_texts(
        title: Option<&str>,
        cwd: Option<&str>,
    ) -> Option<PaneLabelTexts> {
        let title = title.map(str::trim).filter(|text| !text.is_empty());
        let cwd = cwd.map(str::trim).filter(|text| !text.is_empty());
        match (title, cwd) {
            (Some(title), Some(cwd)) if !Self::same_path_text(title, cwd) => Some(PaneLabelTexts {
                left: title.to_string(),
                right: Some(cwd.to_string()),
            }),
            (title, cwd) => cwd.or(title).map(|text| PaneLabelTexts {
                left: text.to_string(),
                right: None,
            }),
        }
    }

    /// 标签页标题：按优先级挑选来源，手动名同样适用。标签标题不带分支，分支只显示在窗格标题上。
    pub(crate) fn resolved_tab_title_base(&self, index: usize) -> String {
        let tab = &self.session.tabs[index];
        let fallback_title = self.fallback_title();
        let smart_mode_shell_fallback = Self::smart_mode_shell_fallback_enabled(&self.tab_title);
        let prediction_allows_shell = self.tab_title.priority.contains(&TabTitleSource::Shell);

        for source in &self.tab_title.priority {
            let candidate = Self::title_source_candidate(
                *source,
                tab.manual_title.as_deref(),
                tab.explicit_title.as_deref(),
                tab.explicit_title_is_prediction,
                prediction_allows_shell,
                tab.shell_title.as_deref(),
                fallback_title,
                smart_mode_shell_fallback,
            );

            if let Some(candidate) = candidate.map(str::trim).filter(|value| !value.is_empty()) {
                return Self::truncate_tab_title(candidate);
            }
        }

        Self::truncate_tab_title(fallback_title)
    }

    pub(crate) fn refresh_tab_title(&mut self, index: usize) -> bool {
        if index >= self.session.tabs.len() {
            return false;
        }

        let next = self.resolved_tab_title_base(index);
        if self.session.tabs[index].title == next {
            return false;
        }

        let previous = std::mem::replace(&mut self.session.tabs[index].title, next);
        let current_title = self.session.tabs[index].title.clone();
        self.invalidate_tab_title_width_cache_for_title(previous.as_str());
        self.invalidate_tab_title_width_cache_for_title(current_title.as_str());

        // Keep title-width behavior uniform across manual, shell, explicit, and fallback sources.
        self.session.tabs[index].sticky_title_width = 0.0;
        self.session.tabs[index].title_text_width = 0.0;
        self.mark_tab_strip_layout_dirty();
        true
    }

    pub(crate) fn apply_terminal_title(
        &mut self,
        index: usize,
        title: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let title = title.trim();
        if title.is_empty() || index >= self.session.tabs.len() {
            return false;
        }

        if let Some(explicit_payload) = self.parse_explicit_title(title) {
            return match explicit_payload {
                ExplicitTitlePayload::Prompt { title, cwd } => {
                    self.session.tabs[index].last_prompt_cwd =
                        Some(Self::normalize_reported_cwd(&cwd));
                    self.session.tabs[index].running_process = false;
                    self.session.tabs[index].current_command = None;
                    self.cancel_pending_command_title(index);
                    self.set_explicit_title(index, title)
                }
                ExplicitTitlePayload::Title(prompt_title) => {
                    self.session.tabs[index].current_command = None;
                    self.cancel_pending_command_title(index);
                    self.set_explicit_title(index, prompt_title)
                }
                ExplicitTitlePayload::Command { title, command } => {
                    self.session.tabs[index].running_process = true;
                    self.session.tabs[index].current_command = Some(command);
                    let tab_id = self.session.tabs[index].id;
                    self.schedule_delayed_command_title(tab_id, title, COMMAND_TITLE_DELAY_MS, cx);
                    false
                }
            };
        }

        let shell_title = Self::truncate_tab_title(title);
        if self.session.tabs[index].shell_title.as_deref() == Some(shell_title.as_str()) {
            return false;
        }

        self.session.tabs[index].shell_title = Some(shell_title);
        self.refresh_tab_title(index)
    }

    pub(crate) fn clear_terminal_titles(&mut self, index: usize) -> bool {
        if index >= self.session.tabs.len() {
            return false;
        }

        self.cancel_pending_command_title(index);
        let tab = &mut self.session.tabs[index];
        tab.running_process = false;
        tab.current_command = None;
        let had_shell = tab.shell_title.take().is_some();
        let had_explicit = tab.explicit_title.take().is_some();
        tab.explicit_title_is_prediction = false;
        if !had_shell && !had_explicit {
            return false;
        }

        self.refresh_tab_title(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TabTitleConfig, TabTitleSource};

    fn expected_home_relative_path(parts: &[&str]) -> String {
        if parts.is_empty() {
            return "~".to_string();
        }

        let separator = std::path::MAIN_SEPARATOR.to_string();
        format!("~{}{}", std::path::MAIN_SEPARATOR, parts.join(&separator))
    }

    fn pane_with(path: &str, command: &str) -> TmuxPaneState {
        TmuxPaneState {
            id: "%1".to_string(),
            window_id: "@1".to_string(),
            session_id: "$1".to_string(),
            is_active: true,
            left: 0,
            top: 0,
            width: 80,
            height: 24,
            cursor_x: 0,
            cursor_y: 0,
            mouse_mode: TmuxPaneMouseMode::default(),
            current_path: path.to_string(),
            current_command: command.to_string(),
        }
    }

    #[test]
    fn predicted_prompt_seed_title_uses_cwd_template_when_explicit_is_enabled() {
        let config = TabTitleConfig::default();
        let title = TerminalView::predicted_prompt_seed_title(&config, Some("~/projects/termy"));
        assert_eq!(title.as_deref(), Some("~/projects/termy"));
    }

    #[test]
    fn predicted_prompt_seed_title_formats_absolute_home_path_relative() {
        let config = TabTitleConfig::default();
        let home = TerminalView::user_home_dir().expect("home dir");
        let cwd = home.join("projects").join("termy");
        let expected = expected_home_relative_path(&["projects", "termy"]);
        let title = TerminalView::predicted_prompt_seed_title(
            &config,
            Some(cwd.to_string_lossy().as_ref()),
        );

        assert_eq!(title.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn predicted_prompt_seed_title_skips_static_only_priority() {
        let config = TabTitleConfig {
            priority: vec![TabTitleSource::Manual, TabTitleSource::Fallback],
            ..Default::default()
        };

        let title = TerminalView::predicted_prompt_seed_title(&config, Some("~/projects/termy"));
        assert!(title.is_none());
    }

    #[test]
    fn predicted_prompt_seed_title_ignores_empty_resolved_output() {
        let config = TabTitleConfig {
            prompt_format: "{cwd}".to_string(),
            ..Default::default()
        };

        let title = TerminalView::predicted_prompt_seed_title(&config, None);
        assert!(title.is_none());
    }

    #[test]
    fn smart_mode_shell_fallback_enabled_when_shell_integration_is_off() {
        let config = TabTitleConfig {
            shell_integration: false,
            ..Default::default()
        };
        assert!(TerminalView::smart_mode_shell_fallback_enabled(&config));
    }

    #[test]
    fn smart_mode_shell_fallback_disabled_when_shell_integration_is_on() {
        let config = TabTitleConfig::default();
        assert!(!TerminalView::smart_mode_shell_fallback_enabled(&config));
    }

    #[test]
    fn smart_mode_shell_fallback_disabled_for_non_smart_mode() {
        let config = TabTitleConfig {
            mode: termy_core::config_core::TabTitleMode::Shell,
            shell_integration: false,
            ..Default::default()
        };
        assert!(!TerminalView::smart_mode_shell_fallback_enabled(&config));
    }

    #[test]
    fn title_source_candidate_prefers_shell_when_smart_shell_fallback_is_enabled() {
        let candidate = TerminalView::title_source_candidate(
            TabTitleSource::Explicit,
            None,
            Some("explicit"),
            false,
            false,
            Some("shell"),
            "fallback",
            true,
        );
        assert_eq!(candidate, Some("shell"));
    }

    #[test]
    fn title_source_candidate_uses_explicit_when_shell_is_unavailable() {
        let candidate = TerminalView::title_source_candidate(
            TabTitleSource::Explicit,
            None,
            Some("explicit"),
            false,
            false,
            None,
            "fallback",
            true,
        );
        assert_eq!(candidate, Some("explicit"));
    }

    #[test]
    fn title_source_candidate_prefers_shell_over_predicted_explicit_title() {
        let candidate = TerminalView::title_source_candidate(
            TabTitleSource::Explicit,
            None,
            Some("predicted"),
            true,
            true,
            Some("shell"),
            "fallback",
            false,
        );
        assert_eq!(candidate, Some("shell"));
    }

    #[test]
    fn title_source_candidate_keeps_predicted_explicit_when_shell_source_is_disabled() {
        let candidate = TerminalView::title_source_candidate(
            TabTitleSource::Explicit,
            None,
            Some("predicted"),
            true,
            false,
            Some("shell"),
            "fallback",
            false,
        );
        assert_eq!(candidate, Some("predicted"));
    }

    #[test]
    fn resolve_template_replaces_known_tokens_in_single_pass() {
        let resolved = TerminalView::resolve_template(
            "cwd={cwd} command={command}",
            Some("{command}"),
            Some("{cwd}"),
        );
        assert_eq!(resolved, "cwd={command} command={cwd}");
    }

    #[test]
    fn resolve_template_leaves_unknown_brace_tokens_unchanged() {
        let resolved =
            TerminalView::resolve_template("start {unknown} end", Some("cwd"), Some("cmd"));
        assert_eq!(resolved, "start {unknown} end");
    }

    #[test]
    fn resolve_prompt_title_formats_absolute_home_paths_relative() {
        let home = TerminalView::user_home_dir().expect("home dir");
        let cwd = home.join("projects").join("termy");
        let expected = expected_home_relative_path(&["projects", "termy"]);

        assert_eq!(
            TerminalView::resolve_prompt_title("{cwd}", Some(cwd.to_string_lossy().as_ref())),
            expected
        );
    }

    #[test]
    fn resolve_prompt_title_leaves_non_home_absolute_paths_unchanged() {
        assert_eq!(
            TerminalView::resolve_prompt_title("{cwd}", Some("/tmp/work")),
            "/tmp/work"
        );
    }

    #[test]
    fn is_shell_command_matches_fixed_shell_set_case_insensitively() {
        assert!(TerminalView::is_shell_command("zsh"));
        assert!(TerminalView::is_shell_command("PwSh"));
        assert!(TerminalView::is_shell_command("/bin/bash"));
        assert!(TerminalView::is_shell_command("-zsh"));
        assert!(TerminalView::is_shell_command("pwsh.exe"));
        assert!(TerminalView::is_shell_command("bash -l"));
        assert!(TerminalView::is_shell_command("cmd"));
        assert!(!TerminalView::is_shell_command("sleep"));
        assert!(!TerminalView::is_shell_command(""));
    }

    #[test]
    fn derive_tmux_shell_title_uses_prompt_format_for_shell_commands() {
        let tab_title = TabTitleConfig {
            prompt_format: "cwd:{cwd}".to_string(),
            ..Default::default()
        };
        let pane = pane_with("/tmp/work", "zsh");

        let title = TerminalView::derive_tmux_shell_title(&tab_title, &pane);
        assert_eq!(title.as_deref(), Some("cwd:/tmp/work"));
    }

    #[test]
    fn derive_tmux_shell_title_formats_home_paths_relative() {
        let tab_title = TabTitleConfig {
            prompt_format: "cwd:{cwd}".to_string(),
            ..Default::default()
        };
        let home = TerminalView::user_home_dir().expect("home dir");
        let pane = pane_with(
            home.join("work").join("project").to_string_lossy().as_ref(),
            "zsh",
        );
        let expected = format!("cwd:{}", expected_home_relative_path(&["work", "project"]));

        let title = TerminalView::derive_tmux_shell_title(&tab_title, &pane);
        assert_eq!(title.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn derive_tmux_shell_title_uses_command_format_for_non_shell_commands() {
        let tab_title = TabTitleConfig {
            command_format: "run:{command}".to_string(),
            ..Default::default()
        };
        let pane = pane_with("/tmp/work", "sleep");

        let title = TerminalView::derive_tmux_shell_title(&tab_title, &pane);
        assert_eq!(title.as_deref(), Some("run:sleep"));
    }

    #[test]
    fn derive_tmux_shell_title_returns_none_when_resolved_title_is_empty() {
        let tab_title = TabTitleConfig {
            command_format: " ".to_string(),
            ..Default::default()
        };
        let pane = pane_with("/tmp/work", "sleep");

        let title = TerminalView::derive_tmux_shell_title(&tab_title, &pane);
        assert!(title.is_none());
    }

    #[allow(clippy::unnecessary_wraps)]
    fn texts(left: &str, right: Option<&str>) -> Option<PaneLabelTexts> {
        Some(PaneLabelTexts {
            left: left.to_string(),
            right: right.map(str::to_string),
        })
    }

    #[test]
    fn pane_label_custom_title_goes_left_and_path_right() {
        assert_eq!(
            TerminalView::pane_label_texts(Some("build"), Some(r"C:\work\app")),
            texts("build", Some(r"C:\work\app"))
        );
    }

    #[test]
    fn pane_label_title_equal_to_path_counts_as_no_title() {
        // 大小写、分隔符、末尾分隔符不同，仍视为同一路径。
        assert_eq!(
            TerminalView::pane_label_texts(Some("c:/work/app/"), Some(r"C:\work\app")),
            texts(r"C:\work\app", None)
        );
    }

    #[test]
    fn pane_label_without_title_uses_path_as_title() {
        assert_eq!(
            TerminalView::pane_label_texts(None, Some(r"C:\work")),
            texts(r"C:\work", None)
        );
        assert_eq!(
            TerminalView::pane_label_texts(Some("  "), Some(r"C:\work")),
            texts(r"C:\work", None)
        );
    }

    #[test]
    fn pane_label_without_known_cwd_shows_title_only() {
        assert_eq!(
            TerminalView::pane_label_texts(Some("vim main.rs"), None),
            texts("vim main.rs", None)
        );
    }

    #[test]
    fn pane_label_is_none_without_any_text() {
        assert_eq!(TerminalView::pane_label_texts(None, None), None);
        assert_eq!(TerminalView::pane_label_texts(Some(""), Some(" ")), None);
    }

    fn map_of(entries: &[(&str, &str)]) -> HashMap<String, PaneMeta> {
        entries
            .iter()
            .map(|(key, value)| {
                let meta = PaneMeta {
                    manual_title: Some(value.to_string()),
                    ..PaneMeta::default()
                };
                (key.to_string(), meta)
            })
            .collect()
    }

    #[test]
    fn only_manual_pane_title_counts_as_custom_title() {
        // 终端上报的标题（cmd.exe 全路径、vim 等）不是自定义标题，没有手动名就是 None。
        let manual = map_of(&[("p1", "build")]);
        assert_eq!(
            TerminalView::resolve_pane_title(&manual, "p1"),
            Some("build")
        );
        assert_eq!(TerminalView::resolve_pane_title(&manual, "p2"), None);
    }

    #[test]
    fn clearing_manual_pane_title_falls_back_to_path() {
        let mut manual = HashMap::new();
        assert!(TerminalView::apply_manual_pane_title(
            &mut manual,
            "p1",
            "  build  "
        ));
        assert_eq!(
            TerminalView::resolve_pane_title(&manual, "p1"),
            Some("build")
        );
        // 重复提交相同名字不算变化。
        assert!(!TerminalView::apply_manual_pane_title(
            &mut manual,
            "p1",
            "build"
        ));
        // 空串清除手动名，没有自定义标题，窗格标签改用当前路径。
        assert!(TerminalView::apply_manual_pane_title(
            &mut manual,
            "p1",
            "   "
        ));
        assert_eq!(TerminalView::resolve_pane_title(&manual, "p1"), None);
        assert_eq!(
            TerminalView::pane_label_texts(
                TerminalView::resolve_pane_title(&manual, "p1"),
                Some(r"C:\Users\jelon")
            ),
            texts(r"C:\Users\jelon", None)
        );
        assert!(!TerminalView::apply_manual_pane_title(
            &mut manual,
            "p1",
            ""
        ));
    }

    #[test]
    fn manual_pane_title_is_truncated_and_single_line() {
        let mut manual = HashMap::new();
        let long = "a".repeat(MAX_TAB_TITLE_CHARS + 20);
        assert!(TerminalView::apply_manual_pane_title(
            &mut manual,
            "p1",
            &long
        ));
        assert_eq!(
            manual["p1"].manual_title.as_ref().unwrap().chars().count(),
            MAX_TAB_TITLE_CHARS
        );
        assert!(TerminalView::apply_manual_pane_title(
            &mut manual,
            "p2",
            "a\nb"
        ));
        assert_eq!(manual["p2"].manual_title.as_deref(), Some("a b"));
    }

    #[test]
    fn msys_cwd_is_converted_to_windows_drive_path() {
        let cases = [
            ("/c/Users/jelon", r"C:\Users\jelon"),
            ("/d", r"D:\"),
            ("/cygdrive/e/work/api", r"E:\work\api"),
            ("/C/Program Files", r"C:\Program Files"),
            ("/E:/work/api", r"E:\work\api"),
            ("/e:", r"E:\"),
            (r"/E:\work\api", r"E:\work\api"),
            ("/C%3A/Users/x", r"C:\Users\x"),
            ("/C:%5CUsers%5Cx", r"C:%5CUsers%5Cx"),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                TerminalView::normalize_drive_style_cwd(raw),
                expected,
                "{raw}"
            );
        }
        // 不是盘符形式的路径保持原样：Windows 路径、UNC、WSL 内部路径、相对路径。
        for raw in [
            r"C:\Users\jelon",
            r"\wsl.localhost\Ubuntu\home",
            "/home/jelon",
            "/usr/bin",
            "relative/x",
            "",
        ] {
            assert_eq!(TerminalView::normalize_drive_style_cwd(raw), raw, "{raw}");
        }
    }

    #[test]
    fn reported_cwd_is_not_truncated_or_whitespace_collapsed() {
        let long = format!("/tmp/{}  x", "d".repeat(MAX_TAB_TITLE_CHARS * 2));
        assert_eq!(
            TerminalView::normalize_reported_cwd(&format!("  \"{long}\"  ")),
            long
        );
        // OSC 9;9 原样路径里的字面 `%XX` 保持不变。
        assert_eq!(
            TerminalView::normalize_reported_cwd("/tmp/a%41"),
            "/tmp/a%41"
        );
        assert_eq!(TerminalView::normalize_reported_cwd("  "), "");
    }

    #[test]
    fn clearing_manual_title_keeps_other_meta_and_drops_empty_entries() {
        let mut metas = HashMap::new();
        metas.insert(
            "p1".to_string(),
            PaneMeta {
                manual_title: Some("build".into()),
                reported_title: Some("vim".into()),
                cwd: None,
                ..PaneMeta::default()
            },
        );
        // 清掉手动名后上报标题还在，条目保留。
        assert!(TerminalView::apply_manual_pane_title(&mut metas, "p1", ""));
        assert_eq!(metas["p1"].reported_title.as_deref(), Some("vim"));
        assert_eq!(TerminalView::resolve_pane_title(&metas, "p1"), None);
        // 上报标题不算自定义标题。
        assert!(PaneMeta::default().manual_title.is_none());
        metas.get_mut("p1").unwrap().reported_title = None;
        assert!(metas["p1"].is_empty());
        // 全空条目在清除手动名时被移除。
        assert!(TerminalView::apply_manual_pane_title(&mut metas, "p2", "x"));
        assert!(TerminalView::apply_manual_pane_title(&mut metas, "p2", ""));
        assert!(!metas.contains_key("p2"));
    }

    #[test]
    fn manual_pane_title_follows_cwd_changes() {
        let mut metas = HashMap::new();
        let cd = |metas: &mut HashMap<String, PaneMeta>, path: &str| {
            TerminalView::apply_pane_cwd(metas, "p1", path.to_string())
        };
        // 先有路径再起名：名字绑定当前路径。
        assert!(cd(&mut metas, "/a"));
        TerminalView::apply_manual_pane_title(&mut metas, "p1", "build");
        assert_eq!(TerminalView::resolve_pane_title(&metas, "p1"), Some("build"));
        // 切到没起过名的目录：回落成路径。
        assert!(cd(&mut metas, "/b"));
        assert_eq!(TerminalView::resolve_pane_title(&metas, "p1"), None);
        // 在新目录起名，再切回旧目录：各自的名字回来。
        TerminalView::apply_manual_pane_title(&mut metas, "p1", "docs");
        assert!(cd(&mut metas, "/a"));
        assert_eq!(TerminalView::resolve_pane_title(&metas, "p1"), Some("build"));
        assert!(cd(&mut metas, "/b"));
        assert_eq!(TerminalView::resolve_pane_title(&metas, "p1"), Some("docs"));
        // 清除名字只清当前路径的绑定。
        TerminalView::apply_manual_pane_title(&mut metas, "p1", "");
        assert!(cd(&mut metas, "/a"));
        assert_eq!(TerminalView::resolve_pane_title(&metas, "p1"), Some("build"));
        assert!(!cd(&mut metas, "/a"));
    }

    #[test]
    fn manual_title_set_before_first_cwd_binds_to_that_path() {
        let mut metas = HashMap::new();
        TerminalView::apply_manual_pane_title(&mut metas, "p1", "build");
        assert!(TerminalView::apply_pane_cwd(&mut metas, "p1", "/a".into()));
        assert_eq!(TerminalView::resolve_pane_title(&metas, "p1"), Some("build"));
        assert!(TerminalView::apply_pane_cwd(&mut metas, "p1", "/b".into()));
        assert_eq!(TerminalView::resolve_pane_title(&metas, "p1"), None);
    }

    #[test]
    fn manual_pane_title_without_cwd_shows_only_left() {
        let manual = map_of(&[("p1", "build")]);
        let title = TerminalView::resolve_pane_title(&manual, "p1");
        assert_eq!(
            TerminalView::pane_label_texts(title, None),
            texts("build", None)
        );
    }
}

use super::*;
use crate::workspace_store::{
    StoredPane, StoredSession, StoredTab, StoredTabCommand, StoredTabPresentation, StoredWorkspace,
    WORKSPACE_STORE_FILE, WorkspaceStore,
};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use termy_core::session_model::PersistedNativeLayoutNode;

/// Legacy JSON state file; read once to seed a fresh SQLite store.
const NATIVE_WORKSPACE_STATE_FILE: &str = "native-tabs.json";

/// 元数据（路径、标题）变化后的保存防抖：比结构变化长，避免高频标题刷新反复重写整库。
const NATIVE_PERSIST_META_DEBOUNCE: Duration = Duration::from_millis(1500);
/// 结构变化（拆分、关闭、改名等）后的保存防抖。
const NATIVE_PERSIST_DEBOUNCE: Duration = Duration::from_millis(80);

/// 读取 JSON 对象里的字符串字段，缺失、非字符串或全空白时返回 `None`。
///
/// - `value`：JSON 对象。
/// - `key`：字段名。
fn non_blank_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
}

fn inclusive_terminal_line_count(range: TerminalLineRange) -> usize {
    usize::try_from(i64::from(range.last_line) - i64::from(range.first_line) + 1).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PersistedNativePane {
    session_id: Option<String>,
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    buffer: Option<String>,
    manual_title: Option<String>,
    /// 窗格最后一次上报的当前目录。
    cwd: Option<String>,
    /// 终端最后一次上报的窗格标题。
    reported_title: Option<String>,
    /// 窗格级自定义指令参数。
    params: std::collections::BTreeMap<String, String>,
    /// 路径 -> 窗格名。
    path_titles: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
struct PersistedNativeTab {
    zoomed: bool,
    presentation: Option<StoredTabPresentation>,
    panes: Vec<PersistedNativePane>,
    layout_tree: Option<PersistedNativeLayoutNode>,
    active_pane: usize,
    pinned: bool,
    /// 标签背景色预设键，没设置为 `None`。
    color: Option<String>,
    /// 标签自定义指令。
    commands: Vec<StoredTabCommand>,
    manual_title: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct PersistedNativeWorkspace {
    tabs: Vec<PersistedNativeTab>,
    active_tab: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct PersistedNamedLayout {
    name: String,
    workspace: PersistedNativeWorkspace,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct PersistedNativeWorkspaceState {
    last_session: Option<PersistedNativeWorkspace>,
    layouts: Vec<PersistedNamedLayout>,
}

/// Rebuilt tabs for one workspace: `(tabs, pane layout trees by tab id,
/// clamped active tab index)`.
type RestoredWorkspaceTabs = (
    Vec<TerminalTab>,
    HashMap<TabId, NativePaneLayoutTree>,
    HashMap<TabId, NativePaneZoomSnapshot>,
    usize,
);

#[derive(Clone)]
struct PersistedNativeWorkspaceWriteRequest {
    store: Option<Arc<WorkspaceStore>>,
    multiplexer: Option<Arc<crate::multiplexer::WindowSession>>,
    session: StoredSession,
    /// `(name, snapshot_json)` of the visible workspace when layout autosave
    /// applies; updates the named layout row if it still exists.
    named_layout_autosave: Option<(String, String)>,
    persist_last_session: bool,
}

pub(super) struct StartupNativeSession {
    pub(super) store: Arc<WorkspaceStore>,
    pub(super) session: Option<StoredSession>,
}

impl TerminalView {
    #[cfg(test)]
    fn extract_persisted_buffer_line(terminal: &Terminal, line_idx: i32) -> Option<String> {
        let mut text = String::new();
        let range =
            terminal.for_each_line_cell_range(line_idx, line_idx, |range, _, _, cell| {
                if text.capacity() == 0 {
                    text.reserve(range.columns);
                }
                let character = cell.character();
                if character == '\0' || cell.is_trailing_wide_spacer() || character.is_control() {
                    text.push(' ');
                } else {
                    text.push(character);
                    cell.append_combining_to(&mut text);
                }
            })?;
        if line_idx < range.first_line || line_idx > range.last_line {
            return None;
        }

        Some(text.trim_end().to_string())
    }

    fn extract_persisted_buffer_text(&self, terminal: &Terminal) -> Option<String> {
        if self.multiplexer.is_some() || !self.native_buffer_persistence {
            return None;
        }

        let mut joined = String::new();
        let mut current_line = None;
        let mut current_line_start = 0usize;
        terminal.for_each_line_cell_range(i32::MIN, i32::MAX, |range, line, _, cell| {
            if current_line != Some(line) {
                if current_line.is_some() {
                    let trimmed = joined[current_line_start..].trim_end().len();
                    joined.truncate(current_line_start + trimmed);
                    joined.push_str("\r\n");
                } else {
                    joined.reserve(
                        inclusive_terminal_line_count(range)
                            .saturating_mul(range.columns.saturating_add(2)),
                    );
                }
                current_line = Some(line);
                current_line_start = joined.len();
            }

            let character = cell.character();
            if character == '\0' || cell.is_trailing_wide_spacer() || character.is_control() {
                joined.push(' ');
            } else {
                joined.push(character);
                cell.append_combining_to(&mut joined);
            }
        })?;
        if current_line.is_some() {
            let trimmed = joined[current_line_start..].trim_end().len();
            joined.truncate(current_line_start + trimmed);
        }
        (!joined.trim().is_empty()).then_some(joined)
    }

    fn should_sync_persisted_native_workspace(&self) -> bool {
        self.runtime_kind() == RuntimeKind::Native
            && (self.multiplexer.is_some()
                || self.should_persist_last_native_session()
                || (self.native_layout_autosave && self.current_named_layout.is_some()))
    }

    /// 是否把当前会话写入 `workspaces.db` 作为“上次会话”。
    /// 开着多路复用器时也要写：布局本体只在宿主进程内存里，宿主被安装器结束后，
    /// 启动只能回退到这份快照。
    pub(in super::super) fn should_persist_last_native_session(&self) -> bool {
        self.owns_persisted_session
            && (self.native_tab_persistence || self.workspace_sidebar_enabled)
    }

    fn persisted_native_workspace_path() -> Result<PathBuf, String> {
        let config_path = crate::config::ensure_config_file().map_err(|error| error.to_string())?;
        let parent = config_path
            .parent()
            .ok_or_else(|| format!("Invalid config path '{}'", config_path.display()))?;
        Ok(parent.join(NATIVE_WORKSPACE_STATE_FILE))
    }

    fn load_persisted_native_workspace_state_from_path(
        path: &std::path::Path,
    ) -> Result<PersistedNativeWorkspaceState, String> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedNativeWorkspaceState::default());
            }
            Err(error) => {
                return Err(format!(
                    "Failed to read workspace state '{}': {}",
                    path.display(),
                    error
                ));
            }
        };

        Self::parse_persisted_native_workspace_state(&contents)
    }

    fn workspace_store_path() -> Result<PathBuf, String> {
        let config_path = crate::config::ensure_config_file().map_err(|error| error.to_string())?;
        let parent = config_path
            .parent()
            .ok_or_else(|| format!("Invalid config path '{}'", config_path.display()))?;
        Ok(parent.join(WORKSPACE_STORE_FILE))
    }

    /// Open (or return the cached) SQLite session store. A fresh database is
    /// seeded from the legacy `native-tabs.json` file when one exists.
    fn workspace_store(&self) -> Option<Arc<WorkspaceStore>> {
        self.workspace_store
            .get_or_init(|| match Self::open_workspace_store() {
                Ok(store) => Some(store),
                Err(error) => {
                    log::error!("Failed to open workspace store: {error}");
                    None
                }
            })
            .clone()
    }

    fn open_workspace_store() -> Result<Arc<WorkspaceStore>, String> {
        let store = Arc::new(WorkspaceStore::open(&Self::workspace_store_path()?)?);
        if store.is_fresh()? {
            Self::import_legacy_native_workspace_state(&store);
        }
        Ok(store)
    }

    pub(super) fn load_startup_native_session(
        config: &AppConfig,
    ) -> Result<Option<StartupNativeSession>, String> {
        if !config.native_tab_persistence && !config.sidebar_enabled {
            return Ok(None);
        }
        let store = Self::open_workspace_store()?;
        let session = store.load_session()?;
        Ok(Some(StartupNativeSession { store, session }))
    }

    fn import_legacy_native_workspace_state(store: &WorkspaceStore) {
        let state = Self::persisted_native_workspace_path()
            .and_then(|path| Self::load_persisted_native_workspace_state_from_path(&path));
        match state {
            Ok(state) => {
                if let Some(last_session) = state.last_session {
                    let session = StoredSession {
                        workspaces: vec![Self::stored_workspace_from_persisted(
                            "Workspace 1".to_string(),
                            last_session,
                        )],
                        active_workspace: 0,
                    };
                    if let Err(error) = store.save_session(&session) {
                        log::warn!("Failed to import legacy session: {error}");
                    }
                }
                for layout in state.layouts {
                    let snapshot = Self::persisted_workspace_to_value(layout.workspace).to_string();
                    if let Err(error) = store.upsert_named_layout(&layout.name, &snapshot) {
                        log::warn!("Failed to import legacy layout '{}': {error}", layout.name);
                    }
                }
            }
            Err(error) => {
                log::warn!("Skipping legacy workspace state import: {error}");
            }
        }
        if let Err(error) = store.mark_initialized() {
            log::warn!("Failed to mark workspace store as initialized: {error}");
        }
    }

    fn stored_workspace_from_persisted(
        name: String,
        workspace: PersistedNativeWorkspace,
    ) -> StoredWorkspace {
        StoredWorkspace {
            name,
            pinned: false,
            active_tab: workspace.active_tab,
            tabs: workspace
                .tabs
                .into_iter()
                .map(|tab| StoredTab {
                    zoomed: tab.zoomed,
                    presentation: tab.presentation,
                    pinned: tab.pinned,
                    color: tab.color,
                    commands: tab.commands,
                    manual_title: tab.manual_title,
                    active_pane: tab.active_pane,
                    layout_tree_json: tab
                        .layout_tree
                        .map(|tree| NativeLayout::persisted_layout_tree_to_value(tree).to_string()),
                    panes: tab
                        .panes
                        .into_iter()
                        .map(|pane| StoredPane {
                            params: pane.params,
                            path_titles: pane.path_titles,
                            manual_title: pane.manual_title,
                            cwd: pane.cwd,
                            reported_title: pane.reported_title,
                            session_id: pane.session_id,
                            left: pane.left,
                            top: pane.top,
                            width: pane.width,
                            height: pane.height,
                            buffer: pane.buffer,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn persisted_workspace_from_stored(
        workspace: StoredWorkspace,
    ) -> (String, bool, PersistedNativeWorkspace) {
        let pinned = workspace.pinned;
        let tabs = workspace
            .tabs
            .into_iter()
            .map(|tab| PersistedNativeTab {
                zoomed: tab.zoomed,
                presentation: tab.presentation,
                pinned: tab.pinned,
                color: tab.color,
                commands: tab.commands,
                manual_title: tab.manual_title,
                active_pane: tab.active_pane,
                layout_tree: tab
                    .layout_tree_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<Value>(json).ok())
                    .and_then(|value| NativeLayout::parse_persisted_layout_tree_value(&value).ok()),
                panes: tab
                    .panes
                    .into_iter()
                    .map(|pane| PersistedNativePane {
                        session_id: pane.session_id,
                        left: pane.left,
                        top: pane.top,
                        width: pane.width,
                        height: pane.height,
                        buffer: pane.buffer,
                        manual_title: pane.manual_title,
                        cwd: pane.cwd,
                        reported_title: pane.reported_title,
                        params: pane.params,
                        path_titles: pane.path_titles,
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        (
            workspace.name,
            pinned,
            PersistedNativeWorkspace {
                tabs,
                active_tab: workspace.active_tab,
            },
        )
    }

    pub(super) fn persisted_native_workspace_working_dir(&self) -> Option<String> {
        termy_core::resolve_launch_working_directory(
            self.configured_working_dir.as_deref(),
            self.terminal_runtime.working_dir_fallback,
        )
        .map(|path| path.to_string_lossy().into_owned())
    }

    /// Snapshot of the visible strip only; used for named layouts.
    fn collect_persisted_native_workspace(&self) -> Option<PersistedNativeWorkspace> {
        if self.runtime_kind() != RuntimeKind::Native {
            return None;
        }
        Some(
            self.collect_persisted_workspace_from_tabs(&self.session.tabs, self.session.active_tab),
        )
    }

    /// Snapshot of every workspace — the visible strip plus stashed
    /// workspaces — preserving grouping and sidebar order.
    fn collect_stored_session(&self) -> Option<StoredSession> {
        if self.runtime_kind() != RuntimeKind::Native {
            return None;
        }

        let mut workspaces = Vec::with_capacity(self.session.workspaces.len());
        let mut active_position = 0;
        for (index, entry) in self.session.workspaces.iter().enumerate() {
            let stored_name = if entry.custom_named {
                entry.name.clone()
            } else {
                String::new()
            };
            if index == self.session.active_workspace {
                active_position = workspaces.len();
            }
            if let Some(mut pending) = Self::pending_stored_workspace(entry) {
                pending.name = stored_name;
                workspaces.push(pending);
                continue;
            }
            let (tabs, active_tab) = if index == self.session.active_workspace {
                (self.session.tabs.as_slice(), self.session.active_tab)
            } else {
                (entry.tabs.as_slice(), entry.active_tab)
            };
            let workspace = self.collect_persisted_workspace_from_tabs(tabs, active_tab);
            // Auto-titled workspaces persist an empty name: their label is
            // derived from tab titles, so only user-chosen names are stored.
            workspaces.push(Self::stored_workspace_from_persisted(
                stored_name,
                workspace,
            ));
            if let Some(stored) = workspaces.last_mut() {
                stored.pinned = entry.pinned;
            }
        }
        if workspaces.is_empty() {
            return None;
        }

        Some(StoredSession {
            active_workspace: active_position,
            workspaces,
        })
    }

    fn pending_stored_workspace(entry: &workspaces::WorkspaceEntry) -> Option<StoredWorkspace> {
        let mut pending = entry.pending_restore.clone()?;
        pending.pinned = entry.pinned;
        Some(pending)
    }

    fn collect_persisted_workspace_from_tabs(
        &self,
        source_tabs: &[TerminalTab],
        active_tab: usize,
    ) -> PersistedNativeWorkspace {
        let tabs = source_tabs
            .iter()
            .map(|tab| {
                let zoom = self.session.native_pane_zoom_snapshots.get(&tab.id);
                let source_panes: Vec<&TerminalPane> = if let Some(zoom) = zoom {
                    let mut panes: Vec<_> = zoom.other_panes.iter().collect();
                    if let Some(active) = tab.panes.first() {
                        panes.insert(zoom.active_original_index.min(panes.len()), active);
                    }
                    panes
                } else {
                    tab.panes.iter().collect()
                };
                let panes = source_panes
                    .iter()
                    .map(|pane| {
                        let (left, top, width, height) = zoom
                            .filter(|zoom| zoom.active_pane_id == pane.id)
                            .map_or((pane.left, pane.top, pane.width, pane.height), |zoom| {
                                zoom.active_pane_geometry
                            });
                        let meta = self.pane_meta.get(&pane.id);
                        PersistedNativePane {
                            session_id: pane.terminal.session_id().map(str::to_owned),
                            left,
                            top,
                            width: width.max(1),
                            height: height.max(1),
                            buffer: self.extract_persisted_buffer_text(&pane.terminal),
                            manual_title: meta.and_then(|meta| meta.manual_title.clone()),
                            cwd: meta.and_then(|meta| meta.cwd.clone()),
                            reported_title: meta.and_then(|meta| meta.reported_title.clone()),
                            params: meta.map(|meta| meta.params.clone()).unwrap_or_default(),
                            path_titles: meta
                                .map(|meta| meta.path_titles.clone())
                                .unwrap_or_default(),
                        }
                    })
                    .collect::<Vec<_>>();
                let pane_indices = source_panes
                    .iter()
                    .enumerate()
                    .map(|(index, pane)| (pane.id.clone(), index))
                    .collect::<HashMap<_, _>>();
                let layout_tree = zoom
                    .and_then(|zoom| zoom.layout_tree.as_ref())
                    .or_else(|| self.session.native_pane_layout_trees.get(&tab.id))
                    .and_then(|tree| {
                        NativeLayout::persisted_layout_tree_from_native(&tree.root, &pane_indices)
                    });
                let active_pane = source_panes
                    .iter()
                    .position(|pane| pane.id == tab.active_pane_id)
                    .unwrap_or(0);
                PersistedNativeTab {
                    zoomed: zoom.is_some(),
                    presentation: self.multiplexer.is_some().then(|| StoredTabPresentation {
                        title: tab.title.clone(),
                        explicit_title: tab.explicit_title.clone(),
                        explicit_title_is_prediction: tab.explicit_title_is_prediction,
                        shell_title: tab.shell_title.clone(),
                        current_command: tab.current_command.clone(),
                        last_prompt_cwd: tab.last_prompt_cwd.clone(),
                        running_process: tab.running_process,
                    }),
                    panes,
                    layout_tree,
                    active_pane,
                    pinned: tab.pinned,
                    color: self
                        .tab_colors
                        .get(&tab.id)
                        .map(|color| color.key().to_string()),
                    commands: self.tab_commands.get(&tab.id).cloned().unwrap_or_default(),
                    manual_title: tab.manual_title.clone(),
                }
            })
            .collect::<Vec<_>>();

        let active_tab = if source_tabs.is_empty() {
            0
        } else {
            active_tab.min(source_tabs.len().saturating_sub(1))
        };
        PersistedNativeWorkspace { tabs, active_tab }
    }

    fn persisted_workspace_to_value(workspace: PersistedNativeWorkspace) -> Value {
        json!({
            "active_tab": workspace.active_tab,
            "tabs": workspace.tabs.into_iter().map(|tab| {
                json!({
                    "active_pane": tab.active_pane,
                    "pinned": tab.pinned,
                    "color": tab.color,
                    "manual_title": tab.manual_title,
                    "layout_tree": tab.layout_tree.map(NativeLayout::persisted_layout_tree_to_value),
                    "panes": tab.panes.into_iter().map(|pane| {
                        json!({
                            "left": pane.left,
                            "top": pane.top,
                            "width": pane.width,
                            "height": pane.height,
                            "buffer": pane.buffer,
                            "manual_title": pane.manual_title,
                            "cwd": pane.cwd,
                            "reported_title": pane.reported_title,
                        })
                    }).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
        })
    }

    fn parse_persisted_native_workspace_value(
        root: &Value,
    ) -> Result<PersistedNativeWorkspace, String> {
        fn value_u16(value: &Value, field: &str) -> Result<u16, String> {
            let raw = value
                .as_u64()
                .ok_or_else(|| format!("workspace field '{field}' must be an unsigned integer"))?;
            u16::try_from(raw).map_err(|_| format!("workspace field '{field}' exceeds u16 range"))
        }

        let tabs_value = root
            .get("tabs")
            .and_then(Value::as_array)
            .ok_or_else(|| "workspace state is missing 'tabs'".to_string())?;
        let mut tabs = Vec::with_capacity(tabs_value.len());
        for (tab_index, tab_value) in tabs_value.iter().enumerate() {
            let panes_value = tab_value
                .get("panes")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("workspace tab {tab_index} is missing 'panes'"))?;
            if panes_value.is_empty() {
                continue;
            }

            let mut panes = Vec::with_capacity(panes_value.len());
            for (pane_index, pane_value) in panes_value.iter().enumerate() {
                panes.push(PersistedNativePane {
                    session_id: None,
                    left: value_u16(
                        pane_value.get("left").ok_or_else(|| {
                            format!("workspace tab {tab_index} pane {pane_index} is missing 'left'")
                        })?,
                        "left",
                    )?,
                    top: value_u16(
                        pane_value.get("top").ok_or_else(|| {
                            format!("workspace tab {tab_index} pane {pane_index} is missing 'top'")
                        })?,
                        "top",
                    )?,
                    width: value_u16(
                        pane_value.get("width").ok_or_else(|| {
                            format!(
                                "workspace tab {tab_index} pane {pane_index} is missing 'width'"
                            )
                        })?,
                        "width",
                    )?
                    .max(1),
                    height: value_u16(
                        pane_value.get("height").ok_or_else(|| {
                            format!(
                                "workspace tab {tab_index} pane {pane_index} is missing 'height'"
                            )
                        })?,
                        "height",
                    )?
                    .max(1),
                    buffer: pane_value
                        .get("buffer")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .filter(|buffer| !buffer.is_empty()),
                    manual_title: non_blank_str(pane_value, "manual_title"),
                    cwd: non_blank_str(pane_value, "cwd"),
                    reported_title: non_blank_str(pane_value, "reported_title"),
                    params: Default::default(),
                    path_titles: Default::default(),
                });
            }

            let active_pane = tab_value
                .get("active_pane")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0)
                .min(panes.len().saturating_sub(1));
            let pinned = tab_value
                .get("pinned")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let color = non_blank_str(tab_value, "color");
            let manual_title = tab_value
                .get("manual_title")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|title| !title.trim().is_empty());
            let layout_tree = tab_value
                .get("layout_tree")
                .filter(|value| !value.is_null())
                .map(NativeLayout::parse_persisted_layout_tree_value)
                .transpose()?;
            tabs.push(PersistedNativeTab {
                zoomed: false,
                presentation: None,
                panes,
                layout_tree,
                active_pane,
                pinned,
                color,
                commands: Vec::new(),
                manual_title,
            });
        }

        let active_tab = root
            .get("active_tab")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0)
            .min(tabs.len().saturating_sub(1));
        let active_tab = if tabs.is_empty() { 0 } else { active_tab };

        Ok(PersistedNativeWorkspace { tabs, active_tab })
    }

    fn parse_persisted_native_workspace_state(
        contents: &str,
    ) -> Result<PersistedNativeWorkspaceState, String> {
        let root: Value = serde_json::from_str(contents)
            .map_err(|error| format!("Invalid native tab workspace JSON: {error}"))?;
        let version = root
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| "workspace state is missing 'version'".to_string())?;

        match version {
            1 => {
                let workspace = Self::parse_persisted_native_workspace_value(&root)?;
                Ok(PersistedNativeWorkspaceState {
                    last_session: Some(workspace),
                    layouts: Vec::new(),
                })
            }
            2 | 3 => {
                let last_session = root
                    .get("last_session")
                    .filter(|value| !value.is_null())
                    .map(Self::parse_persisted_native_workspace_value)
                    .transpose()?;
                let layouts_value = root
                    .get("layouts")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "workspace state is missing 'layouts'".to_string())?;
                let mut layouts = Vec::with_capacity(layouts_value.len());
                for (layout_index, layout_value) in layouts_value.iter().enumerate() {
                    let name = layout_value
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| format!("saved layout {layout_index} is missing 'name'"))?
                        .to_string();
                    let workspace = Self::parse_persisted_native_workspace_value(
                        layout_value.get("workspace").ok_or_else(|| {
                            format!("saved layout '{name}' is missing 'workspace'")
                        })?,
                    )?;
                    layouts.push(PersistedNamedLayout { name, workspace });
                }
                layouts.sort_unstable_by_key(|layout| layout.name.to_ascii_lowercase());
                layouts.dedup_by(|left, right| left.name.eq_ignore_ascii_case(&right.name));
                Ok(PersistedNativeWorkspaceState {
                    last_session,
                    layouts,
                })
            }
            _ => Err(format!("Unsupported workspace state version {version}")),
        }
    }

    fn restored_pane_id(tab_id: TabId, pane_index: usize) -> String {
        if pane_index == 0 {
            format!("%native-{tab_id}")
        } else {
            format!("%native-restored-{tab_id}-{}", pane_index + 1)
        }
    }

    fn build_restored_pane(
        &self,
        tab_id: TabId,
        pane_index: usize,
        pane: &PersistedNativePane,
        working_dir: Option<&str>,
    ) -> Result<TerminalPane, String> {
        let pane_id = Self::restored_pane_id(tab_id, pane_index);
        let width = pane.width.max(1);
        let height = pane.height.max(1);
        // 优先用该窗格自己保存的目录启动；目录已不存在时退回统一的启动目录。
        let working_dir = pane
            .cwd
            .as_deref()
            .filter(|dir| std::path::Path::new(dir).is_dir())
            .or(working_dir);
        let terminal = if let (Some(client), Some(id)) =
            (self.multiplexer_client(), pane.session_id.as_deref())
        {
            Terminal::attach_native_session(
                id,
                client,
                &self.native_terminal_wakeup_router,
                &self.terminal_runtime,
            )
            .map_err(|error| format!("Failed to reattach saved pane: {error}"))?
        } else {
            Terminal::new_native(
                TerminalSize {
                    cols: width,
                    rows: height,
                    ..TerminalSize::default()
                },
                working_dir,
                Some(&self.native_terminal_wakeup_router),
                Some(&self.tab_shell_integration),
                Some(&self.terminal_runtime),
                None,
                self.multiplexer_client(),
            )
            .map_err(|error| format!("Failed to restore saved pane: {error}"))?
        };
        if pane.session_id.is_none()
            && self.native_buffer_persistence
            && let Some(buffer) = pane.buffer.as_deref()
        {
            terminal.hydrate_output(buffer.as_bytes());
        }
        Ok(TerminalPane::new_native(
            pane_id, pane.left, pane.top, width, height, terminal,
        ))
    }

    fn create_restored_tab(
        tab_id: TabId,
        first_pane: TerminalPane,
        predicted_prompt_title: Option<String>,
        manual_title: Option<&str>,
    ) -> TerminalTab {
        let title = manual_title
            .filter(|title| !title.trim().is_empty())
            .or(predicted_prompt_title.as_deref())
            .unwrap_or(DEFAULT_TAB_TITLE)
            .to_string();
        let title_text_width = 0.0;
        let sticky_title_width = Self::tab_display_width_for_text_px_without_close_with_max(
            title_text_width,
            TAB_MAX_WIDTH,
        );
        let display_width =
            Self::tab_display_width_for_text_px_with_max(title_text_width, TAB_MAX_WIDTH);
        let active_pane_id = first_pane.id.clone();
        let explicit_title = predicted_prompt_title;
        let explicit_title_is_prediction = explicit_title.is_some();
        TerminalTab {
            id: tab_id,
            window_id: format!("@native-{tab_id}"),
            window_index: 0,
            panes: vec![first_pane],
            active_pane_id,
            pinned: false,
            manual_title: None,
            explicit_title,
            explicit_title_is_prediction,
            shell_title: None,
            current_command: None,
            pending_command_title: None,
            pending_command_token: 0,
            last_prompt_cwd: None,
            title,
            title_text_width,
            sticky_title_width,
            display_width,
            running_process: false,
            command_lifecycle: CommandLifecycle::default(),
        }
    }

    /// Rebuild tabs for one persisted workspace without touching view state.
    fn build_restored_tabs(
        &mut self,
        workspace: PersistedNativeWorkspace,
    ) -> Result<RestoredWorkspaceTabs, String> {
        let working_dir = self.persisted_native_workspace_working_dir();
        let predicted_prompt_cwd = Self::predicted_prompt_cwd(
            working_dir.as_deref(),
            self.terminal_runtime.working_dir_fallback,
        );
        let predicted_title =
            Self::predicted_prompt_seed_title(&self.tab_title, predicted_prompt_cwd.as_deref());
        let mut restored_tabs = Vec::with_capacity(workspace.tabs.len());
        let mut restored_layout_trees = HashMap::new();
        let mut restored_zoom = HashMap::new();

        for persisted_tab in workspace.tabs {
            let first_pane = persisted_tab
                .panes
                .first()
                .ok_or_else(|| "workspace tab is missing panes".to_string())?;
            let tab_id = self.allocate_tab_id();
            let first_pane =
                self.build_restored_pane(tab_id, 0, first_pane, working_dir.as_deref())?;
            let manual_title = persisted_tab.manual_title.clone();
            let mut tab = Self::create_restored_tab(
                tab_id,
                first_pane,
                predicted_title.clone(),
                manual_title.as_deref(),
            );

            for (pane_index, pane) in persisted_tab.panes.iter().enumerate().skip(1) {
                tab.panes.push(self.build_restored_pane(
                    tab_id,
                    pane_index,
                    pane,
                    working_dir.as_deref(),
                )?);
            }

            tab.active_pane_id = tab
                .panes
                .get(persisted_tab.active_pane)
                .or_else(|| tab.panes.first())
                .map(|pane| pane.id.clone())
                .ok_or_else(|| "restored tab has no panes".to_string())?;
            tab.pinned = persisted_tab.pinned;
            if let Some(color) = persisted_tab
                .color
                .as_deref()
                .and_then(tab_colors::TabColor::from_key)
            {
                self.tab_colors.insert(tab.id, color);
            }
            if !persisted_tab.commands.is_empty() {
                self.tab_commands
                    .insert(tab.id, persisted_tab.commands.clone());
            }
            tab.manual_title = manual_title;
            // 手动窗格名、路径、上报标题按新建窗格的 id 重新映射回来。
            for (pane, persisted_pane) in tab.panes.iter().zip(persisted_tab.panes.iter()) {
                if !persisted_pane.path_titles.is_empty() {
                    self.pane_meta
                        .entry(pane.id.clone())
                        .or_default()
                        .path_titles = persisted_pane.path_titles.clone();
                }
                if let Some(title) = persisted_pane.manual_title.as_deref() {
                    self.set_pane_manual_title(pane.id.as_str(), title);
                }
                if let Some(cwd) = persisted_pane.cwd.as_deref() {
                    self.record_pane_cwd(pane.id.as_str(), cwd);
                }
                if let Some(title) = persisted_pane.reported_title.as_deref() {
                    self.record_pane_title(pane.id.as_str(), title);
                }
                if !persisted_pane.params.is_empty() {
                    self.pane_meta.entry(pane.id.clone()).or_default().params =
                        persisted_pane.params.clone();
                }
            }
            if let Some(presentation) = persisted_tab.presentation {
                tab.title = presentation.title;
                tab.explicit_title = presentation.explicit_title;
                tab.explicit_title_is_prediction = presentation.explicit_title_is_prediction;
                tab.shell_title = presentation.shell_title;
                tab.current_command = presentation.current_command;
                tab.last_prompt_cwd = presentation.last_prompt_cwd;
                tab.running_process = presentation.running_process;
            }
            let pane_ids = tab
                .panes
                .iter()
                .map(|pane| pane.id.clone())
                .collect::<Vec<_>>();
            let layout_tree = persisted_tab
                .layout_tree
                .as_ref()
                .and_then(|tree| NativeLayout::native_layout_tree_from_persisted(tree, &pane_ids))
                .map(|root| NativePaneLayoutTree { root })
                .or_else(|| Self::native_layout_tree_from_panes(&tab.panes));
            if let Some(layout_tree) = layout_tree {
                restored_layout_trees.insert(tab.id, layout_tree);
            }
            if persisted_tab.zoomed && tab.panes.len() > 1 {
                let active = tab.active_pane_index().unwrap_or(0);
                let cols = tab
                    .panes
                    .iter()
                    .map(|pane| pane.left.saturating_add(pane.width))
                    .max()
                    .unwrap_or(1);
                let rows = tab
                    .panes
                    .iter()
                    .map(|pane| pane.top.saturating_add(pane.height))
                    .max()
                    .unwrap_or(1);
                let mut panes = std::mem::take(&mut tab.panes);
                let mut active_pane = panes.remove(active);
                let geometry = (
                    active_pane.left,
                    active_pane.top,
                    active_pane.width,
                    active_pane.height,
                );
                active_pane.left = 0;
                active_pane.top = 0;
                active_pane.width = cols;
                active_pane.height = rows;
                tab.panes.push(active_pane);
                restored_zoom.insert(
                    tab.id,
                    NativePaneZoomSnapshot {
                        other_panes: panes,
                        active_pane_geometry: geometry,
                        active_pane_id: tab.active_pane_id.clone(),
                        active_original_index: active,
                        layout_tree: restored_layout_trees.remove(&tab.id),
                    },
                );
            }
            restored_tabs.push(tab);
        }

        let active_tab = if restored_tabs.is_empty() {
            0
        } else {
            workspace
                .active_tab
                .min(restored_tabs.len().saturating_sub(1))
        };
        // Failed restoration must only detach from existing sessions. Arm
        // explicit-close ownership once every pane in the workspace attached.
        for pane in restored_tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .chain(restored_zoom.values().flat_map(|zoom| &zoom.other_panes))
        {
            pane.terminal.adopt_session();
        }
        Ok((
            restored_tabs,
            restored_layout_trees,
            restored_zoom,
            active_tab,
        ))
    }

    pub(super) fn materialize_pending_workspace(&mut self, index: usize) -> Result<bool, String> {
        let Some(stored) = self
            .session
            .workspaces
            .get_mut(index)
            .and_then(|entry| entry.pending_restore.take())
        else {
            return Ok(false);
        };
        let (_, _, workspace) = Self::persisted_workspace_from_stored(stored.clone());
        let (tabs, layout_trees, zoom, active_tab) = match self.build_restored_tabs(workspace) {
            Ok(restored) => restored,
            Err(error) => {
                if let Some(entry) = self.session.workspaces.get_mut(index) {
                    entry.pending_restore = Some(stored);
                }
                return Err(error);
            }
        };
        self.session.native_pane_layout_trees.extend(layout_trees);
        self.session.native_pane_zoom_snapshots.extend(zoom);
        let entry = self
            .session
            .workspaces
            .get_mut(index)
            .ok_or_else(|| "saved workspace disappeared while starting".to_string())?;
        entry.tabs = tabs;
        entry.active_tab = active_tab;
        Ok(true)
    }

    fn pending_workspace_entry_from_stored(
        id: u64,
        stored_workspace: StoredWorkspace,
    ) -> workspaces::WorkspaceEntry {
        let stored_name = stored_workspace.name.clone();
        let custom_named = !stored_name.is_empty()
            && !workspaces::WorkspaceEntry::is_default_workspace_name(&stored_name);
        let fallback_name = stored_workspace
            .tabs
            .get(
                stored_workspace
                    .active_tab
                    .min(stored_workspace.tabs.len().saturating_sub(1)),
            )
            .and_then(|tab| tab.manual_title.as_deref())
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(|| format!("Workspace {id}"), str::to_string);
        workspaces::WorkspaceEntry {
            id,
            name: if stored_name.is_empty() {
                fallback_name
            } else {
                stored_name
            },
            custom_named,
            pinned: stored_workspace.pinned,
            tabs: Vec::new(),
            active_tab: stored_workspace.active_tab,
            pending_restore: Some(stored_workspace),
            attention: false,
        }
    }

    /// Replace the visible strip with one restored workspace (named layout
    /// loads). Stashed workspaces are left untouched.
    /// 复制指定标签页：沿用布局树、窗格尺寸、各窗格目录与手动标题、固定状态、标签标题与颜色，
    /// 每个窗格在原目录里新开 shell（运行中的进程和滚动缓冲区不复制）。
    /// 新标签插在原标签右侧并激活；返回是否复制成功。
    pub(super) fn duplicate_tab_by_id(&mut self, tab_id: TabId, cx: &mut Context<Self>) -> bool {
        // tmux 的标签由服务端维护，本地复制只会造出与服务端不一致的标签。
        if self.runtime_kind() != RuntimeKind::Native {
            return false;
        }
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return false;
        };
        let mut workspace =
            self.collect_persisted_workspace_from_tabs(&self.session.tabs[index..=index], 0);
        for pane in workspace
            .tabs
            .iter_mut()
            .flat_map(|tab| tab.panes.iter_mut())
        {
            pane.session_id = None;
            pane.buffer = None;
        }
        for presentation in workspace
            .tabs
            .iter_mut()
            .filter_map(|tab| tab.presentation.as_mut())
        {
            presentation.running_process = false;
            presentation.current_command = None;
        }
        let (mut tabs, layout_trees, zoom, _) = match self.build_restored_tabs(workspace) {
            Ok(restored) => restored,
            Err(error) => {
                log::warn!("Failed to duplicate tab: {error}");
                crate::ui::toast::error(t!("Failed to duplicate tab"));
                return false;
            }
        };
        let Some(new_tab) = tabs.pop() else {
            return false;
        };
        let new_id = new_tab.id;
        if let Some(color) = self.tab_colors.get(&tab_id).copied() {
            self.tab_colors.insert(new_id, color);
        }
        if let Some(commands) = self.tab_commands.get(&tab_id).cloned() {
            self.tab_commands.insert(new_id, commands);
        }
        self.session.native_pane_layout_trees.extend(layout_trees);
        self.session.native_pane_zoom_snapshots.extend(zoom);
        self.session.tabs.insert(index + 1, new_tab);
        self.session.active_tab = index + 1;
        // 插入点右侧的标签整体后移一位，行内重命名与悬停索引要跟着平移。
        let shift_after_insert = |slot: &mut Option<usize>| {
            if let Some(slot_index) = slot.as_mut().filter(|slot_index| **slot_index > index) {
                *slot_index += 1;
            }
        };
        shift_after_insert(&mut self.renaming_tab);
        shift_after_insert(&mut self.tab_strip.hovered_tab);
        shift_after_insert(&mut self.tab_strip.hovered_tab_close);
        self.finish_workspace_restore(cx);
        self.schedule_persist_native_workspace(cx);
        true
    }

    fn restore_workspace(
        &mut self,
        workspace: PersistedNativeWorkspace,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (tabs, layout_trees, zoom, active_tab) = self.build_restored_tabs(workspace)?;
        for tab in &self.session.tabs {
            self.session.native_pane_layout_trees.remove(&tab.id);
            self.session.native_pane_zoom_snapshots.remove(&tab.id);
        }
        self.session.tabs = tabs;
        self.session.native_pane_layout_trees.extend(layout_trees);
        self.session.native_pane_zoom_snapshots.extend(zoom);
        self.session.active_tab = active_tab;
        self.finish_workspace_restore(cx);
        Ok(())
    }

    /// Rebuild the full workspace set from a stored session: the active
    /// workspace becomes the visible strip, the rest are stashed.
    pub(super) fn restore_stored_session(
        &mut self,
        session: StoredSession,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let stored_active = session
            .active_workspace
            .min(session.workspaces.len().saturating_sub(1));
        let mut entries = Vec::with_capacity(session.workspaces.len());
        for (index, stored_workspace) in session.workspaces.into_iter().enumerate() {
            let id = index as u64 + 1;
            entries.push(Self::pending_workspace_entry_from_stored(
                id,
                stored_workspace,
            ));
        }

        if entries.is_empty() {
            return Err("session state does not contain any restorable workspaces".to_string());
        }

        let mut candidate_indices = std::iter::once(stored_active)
            .chain((0..entries.len()).filter(|index| *index != stored_active));
        let mut restored_active_id = None;
        let mut restored_layout_trees = HashMap::new();
        let mut restored_zoom = HashMap::new();
        for index in candidate_indices.by_ref() {
            let stored_workspace = entries[index]
                .pending_restore
                .take()
                .expect("restore candidate must retain its stored workspace");
            let workspace_name = entries[index].name.clone();
            let (_, _, workspace) = Self::persisted_workspace_from_stored(stored_workspace.clone());
            match self.build_restored_tabs(workspace) {
                Ok((tabs, layout_trees, zoom, active_tab)) => {
                    entries[index].tabs = tabs;
                    entries[index].active_tab = active_tab;
                    restored_layout_trees = layout_trees;
                    restored_zoom = zoom;
                    restored_active_id = Some(entries[index].id);
                    break;
                }
                Err(error) => {
                    if self.multiplexer.is_some() {
                        entries[index].pending_restore = Some(stored_workspace);
                    }
                    log::warn!(
                        "Skipping workspace '{workspace_name}' during session restore: {error}"
                    );
                }
            }
        }
        let restored_active_id = restored_active_id.ok_or_else(|| {
            "session state does not contain any restorable workspaces".to_string()
        })?;
        entries.retain(|entry| entry.id == restored_active_id || entry.pending_restore.is_some());
        let active_entry = entries
            .iter()
            .position(|entry| entry.id == restored_active_id)
            .expect("restored active workspace must be retained");
        self.session.next_workspace_id = entries
            .iter()
            .map(|entry| entry.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.session.workspaces = entries;
        self.session.active_workspace = active_entry;
        let (tabs, active_tab) = {
            let entry = &mut self.session.workspaces[active_entry];
            (std::mem::take(&mut entry.tabs), entry.active_tab)
        };
        self.session.tabs = tabs;
        self.session.active_tab = active_tab.min(self.session.tabs.len().saturating_sub(1));
        self.session.native_pane_layout_trees = restored_layout_trees;
        self.session.native_pane_zoom_snapshots = restored_zoom;
        self.finish_workspace_restore(cx);
        Ok(())
    }

    fn finish_workspace_restore(&mut self, cx: &mut Context<Self>) {
        self.mark_tab_strip_layout_dirty();
        self.sync_tab_strip_for_active_tab();
        self.sync_plugin_lifecycle_state(false, cx);
        // 恢复出来的窗格 cwd 也要挂上 git 监听，标题才有分支后缀。
        self.sync_git_watch(cx);
        for index in 0..self.session.tabs.len() {
            self.refresh_tab_title(index);
        }
        self.clear_selection();
        self.clear_hovered_link();
        cx.notify();
    }

    fn apply_persisted_native_workspace_write_request(
        request: PersistedNativeWorkspaceWriteRequest,
    ) -> Result<(), String> {
        if let Some(window) = &request.multiplexer {
            window.save(request.session.clone())?;
        }
        if let Some(store) = request.store {
            if let Some((name, snapshot)) = &request.named_layout_autosave {
                store.update_named_layout_if_exists(name, snapshot)?;
            }
            if request.persist_last_session {
                store.save_session(&request.session)?;
            }
        }
        Ok(())
    }

    fn persisted_native_workspace_write_request(
        &self,
    ) -> Option<PersistedNativeWorkspaceWriteRequest> {
        if !self.should_sync_persisted_native_workspace() {
            return None;
        }
        let session = self.collect_stored_session()?;
        let store = if self.should_persist_last_native_session() || self.native_layout_autosave {
            Some(self.workspace_store()?)
        } else {
            None
        };
        let named_layout_autosave = if self.native_layout_autosave {
            self.current_named_layout.as_ref().and_then(|name| {
                let snapshot = self
                    .collect_persisted_native_workspace()
                    .map(|workspace| Self::persisted_workspace_to_value(workspace).to_string())?;
                Some((name.clone(), snapshot))
            })
        } else {
            None
        };
        Some(PersistedNativeWorkspaceWriteRequest {
            store,
            multiplexer: self.multiplexer.clone(),
            session,
            named_layout_autosave,
            persist_last_session: self.should_persist_last_native_session(),
        })
    }

    pub(in super::super) fn sync_persisted_native_workspace(&self) {
        // Cancel any still-debouncing write before collecting the state for this
        // synchronous flush.
        self.native_persist_revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.native_persist_meta_revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let Some(request) = self.persisted_native_workspace_write_request() else {
            return;
        };
        let _write_guard = self
            .native_persist_write_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = Self::apply_persisted_native_workspace_write_request(request) {
            log::error!("Failed to persist native tab workspace: {error}");
        }
    }

    pub(in super::super) fn schedule_persist_native_workspace(&self, cx: &mut Context<Self>) {
        self.schedule_persist_native_workspace_after(
            NATIVE_PERSIST_DEBOUNCE,
            &self.native_persist_revision,
            cx,
        );
    }

    /// 窗格路径或上报标题变化后的保存：用更长的防抖合并高频刷新。
    /// 使用独立的版本号，不会被结构类保存顶掉，也不会反过来把它们推迟。
    pub(in super::super) fn schedule_persist_native_pane_meta(&self, cx: &mut Context<Self>) {
        self.schedule_persist_native_workspace_after(
            NATIVE_PERSIST_META_DEBOUNCE,
            &self.native_persist_meta_revision,
            cx,
        );
    }

    /// 按给定防抖时长延迟合并保存；同一版本号计数器内后到的请求会顶掉先到的。
    ///
    /// - `debounce`：防抖时长。
    /// - `revision`：本类保存专用的版本号计数器（结构类与窗格元数据类各用一个）。
    fn schedule_persist_native_workspace_after(
        &self,
        debounce: Duration,
        revision: &Arc<AtomicU64>,
        cx: &mut Context<Self>,
    ) {
        let next_revision = revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1);
        if !self.should_sync_persisted_native_workspace() {
            return;
        }

        let latest_revision = revision.clone();
        let write_gate = self.native_persist_write_gate.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(debounce).await;
            if latest_revision.load(std::sync::atomic::Ordering::Acquire) != next_revision {
                return;
            }

            // Collect only after the debounce window. Session snapshots may include
            // every pane's scrollback, so stale requests must not retain those copies.
            let mut request = None;
            if cx
                .update(|cx| {
                    this.update(cx, |view, _| {
                        if latest_revision.load(std::sync::atomic::Ordering::Acquire)
                            == next_revision
                        {
                            request = view.persisted_native_workspace_write_request();
                        }
                    })
                })
                .is_err()
            {
                return;
            }
            let Some(request) = request else {
                return;
            };

            let result = cx
                .background_executor()
                .spawn(async move {
                    let _write_guard = write_gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if latest_revision.load(std::sync::atomic::Ordering::Acquire) != next_revision {
                        return Ok(());
                    }
                    TerminalView::apply_persisted_native_workspace_write_request(request)
                })
                .await;
            if let Err(error) = result {
                log::error!("Failed to persist native tab workspace: {error}");
            }
        })
        .detach();
    }

    fn require_workspace_store(&self) -> Result<Arc<WorkspaceStore>, String> {
        self.workspace_store()
            .ok_or_else(|| t!("The workspace store is unavailable").to_string())
    }

    pub(in super::super) fn clear_persisted_native_workspace(&self) -> Result<(), String> {
        self.require_workspace_store()?.clear_session()
    }

    pub(in super::super) fn rewrite_persisted_native_workspace_without_buffers(
        &self,
    ) -> Result<(), String> {
        let store = self.require_workspace_store()?;
        store.strip_session_buffers()?;
        // Named layout snapshots are opaque JSON blobs; strip their pane
        // buffers by round-tripping through the persisted representation.
        for (name, snapshot) in store.all_named_layouts()? {
            let value = serde_json::from_str::<Value>(&snapshot)
                .map_err(|error| format!("Invalid saved layout '{name}': {error}"))?;
            let mut workspace = Self::parse_persisted_native_workspace_value(&value)?;
            for tab in &mut workspace.tabs {
                for pane in &mut tab.panes {
                    pane.buffer = None;
                }
            }
            let stripped = Self::persisted_workspace_to_value(workspace).to_string();
            store.upsert_named_layout(&name, &stripped)?;
        }
        Ok(())
    }

    pub(in super::super) fn saved_layout_names(&self) -> Result<Vec<String>, String> {
        self.require_workspace_store()?.named_layout_names()
    }

    pub(in super::super) fn save_current_workspace_as_named_layout(
        &mut self,
        layout_name: &str,
    ) -> Result<(), String> {
        if self.runtime_kind() != RuntimeKind::Native {
            return Err(t!("Saved layouts are only available in the native runtime").to_string());
        }
        let layout_name = layout_name.trim();
        if layout_name.is_empty() {
            return Err(t!("Layout name is required").to_string());
        }
        let workspace = self
            .collect_persisted_native_workspace()
            .ok_or_else(|| t!("There is no native layout to save").to_string())?;
        let snapshot = Self::persisted_workspace_to_value(workspace).to_string();
        self.require_workspace_store()?
            .upsert_named_layout(layout_name, &snapshot)?;
        self.current_named_layout = Some(layout_name.to_string());
        Ok(())
    }

    pub(in super::super) fn load_named_layout(
        &mut self,
        layout_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.runtime_kind() != RuntimeKind::Native {
            return Err(t!("Saved layouts are only available in the native runtime").to_string());
        }
        let (canonical_name, snapshot) = self
            .require_workspace_store()?
            .named_layout(layout_name)?
            .ok_or_else(|| {
                t!(
                    "Saved layout \"{layout_name}\" was not found",
                    layout_name = layout_name
                )
            })?;
        let value = serde_json::from_str::<Value>(&snapshot)
            .map_err(|error| format!("Invalid saved layout '{canonical_name}': {error}"))?;
        let workspace = Self::parse_persisted_native_workspace_value(&value)?;
        self.restore_workspace(workspace, cx)?;
        self.current_named_layout = Some(canonical_name);
        self.sync_persisted_native_workspace();
        Ok(())
    }

    pub(in super::super) fn rename_named_layout(
        &mut self,
        current_layout_name: &str,
        next_layout_name: &str,
    ) -> Result<(), String> {
        let current_layout_name = current_layout_name.trim();
        let next_layout_name = next_layout_name.trim();
        if current_layout_name.is_empty() || next_layout_name.is_empty() {
            return Err(t!("Layout name is required").to_string());
        }

        self.require_workspace_store()?
            .rename_named_layout(current_layout_name, next_layout_name)?;
        let update_current_named_layout = self
            .current_named_layout
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(current_layout_name));
        if update_current_named_layout {
            self.current_named_layout = Some(next_layout_name.to_string());
        }
        Ok(())
    }

    pub(in super::super) fn delete_named_layout(
        &mut self,
        layout_name: &str,
    ) -> Result<(), String> {
        let layout_name = layout_name.trim();
        if layout_name.is_empty() {
            return Err(t!("Layout name is required").to_string());
        }
        if !self
            .require_workspace_store()?
            .delete_named_layout(layout_name)?
        {
            return Err(t!(
                "Saved layout \"{layout_name}\" was not found",
                layout_name = layout_name
            ));
        }
        let clear_current_named_layout = self
            .current_named_layout
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(layout_name));
        if clear_current_named_layout {
            self.current_named_layout = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{PersistedNativeLayoutNode, Terminal, TerminalSize, TerminalView};
    use crate::terminal_view::PaneResizeAxis;
    use crate::workspace_store::{StoredPane, StoredTab, StoredWorkspace};

    #[test]
    fn persisted_core_buffer_preserves_combining_characters_in_history() {
        let size = TerminalSize {
            cols: 4,
            rows: 2,
            ..TerminalSize::default()
        };
        let terminal = Terminal::new_test_display(size);
        terminal.hydrate_output("e\u{301}\r\nmid\r\nnew".as_bytes());

        assert_eq!(
            TerminalView::extract_persisted_buffer_line(&terminal, -1),
            Some("e\u{301}".to_string())
        );
    }

    #[test]
    fn restored_inactive_workspace_keeps_pty_descriptors_cold() {
        let stored = StoredWorkspace {
            name: String::new(),
            pinned: true,
            active_tab: 0,
            tabs: vec![StoredTab {
                commands: Vec::new(),
                zoomed: false,
                presentation: None,
                pinned: false,
                color: None,
                manual_title: Some("Build".to_string()),
                active_pane: 0,
                layout_tree_json: None,
                panes: vec![StoredPane {
                    params: Default::default(),
                    path_titles: Default::default(),
                    manual_title: None,
                    cwd: None,
                    reported_title: None,
                    session_id: None,
                    left: 0,
                    top: 0,
                    width: 80,
                    height: 24,
                    buffer: Some("saved output".to_string()),
                }],
            }],
        };

        let mut entry = TerminalView::pending_workspace_entry_from_stored(2, stored.clone());
        assert!(
            entry.tabs.is_empty(),
            "inactive PTYs must not start at launch"
        );
        assert_eq!(entry.name, "Build");
        assert!(!entry.custom_named);
        assert_eq!(entry.active_tab, 0);
        assert_eq!(entry.pending_restore.as_ref(), Some(&stored));

        entry.pinned = false;
        let pending =
            TerminalView::pending_stored_workspace(&entry).expect("pending workspace snapshot");
        assert!(
            !pending.pinned,
            "sidebar edits must reach the cold snapshot"
        );
        assert_eq!(
            pending.tabs[0].panes[0].buffer.as_deref(),
            Some("saved output")
        );
    }

    #[test]
    fn persisted_native_workspace_parser_accepts_legacy_v1_shape() {
        let state = TerminalView::parse_persisted_native_workspace_state(
            r#"{
  "version": 1,
  "active_tab": 1,
  "tabs": [
    {
      "active_pane": 0,
      "manual_title": "Work",
      "panes": [
        { "left": 0, "top": 0, "width": 60, "height": 20 }
      ]
    },
    {
      "active_pane": 1,
      "manual_title": null,
      "panes": [
        { "left": 0, "top": 0, "width": 40, "height": 20 },
        { "left": 40, "top": 0, "width": 40, "height": 20 }
      ]
    }
  ]
}"#,
        )
        .expect("workspace should parse");

        let workspace = state
            .last_session
            .expect("legacy state should populate last session");
        assert_eq!(workspace.tabs.len(), 2);
        assert_eq!(workspace.active_tab, 1);
        assert!(!workspace.tabs[0].pinned);
        assert_eq!(workspace.tabs[0].manual_title.as_deref(), Some("Work"));
        assert_eq!(workspace.tabs[1].active_pane, 1);
        assert_eq!(workspace.tabs[1].panes[1].left, 40);
    }

    #[test]
    fn persisted_native_workspace_parser_reads_pane_manual_title() {
        let state = TerminalView::parse_persisted_native_workspace_state(
            r#"{
  "version": 1,
  "active_tab": 0,
  "tabs": [
    {
      "active_pane": 0,
      "manual_title": null,
      "panes": [
        { "left": 0, "top": 0, "width": 40, "height": 20, "manual_title": "build", "cwd": "/work/api", "reported_title": "npm run dev" },
        { "left": 40, "top": 0, "width": 40, "height": 20, "manual_title": "  " },
        { "left": 80, "top": 0, "width": 40, "height": 20 }
      ]
    }
  ]
}"#,
        )
        .expect("workspace should parse");

        let panes = &state.last_session.expect("last session").tabs[0].panes;
        assert_eq!(panes[0].manual_title.as_deref(), Some("build"));
        // 空白名字与缺字段（旧数据）都视为没有手动名。
        assert_eq!(panes[1].manual_title, None);
        assert_eq!(panes[2].manual_title, None);
        // 路径和上报标题随窗格读回；缺字段（旧数据）为 None。
        assert_eq!(panes[0].cwd.as_deref(), Some("/work/api"));
        assert_eq!(panes[0].reported_title.as_deref(), Some("npm run dev"));
        assert_eq!(panes[2].cwd, None);
        assert_eq!(panes[2].reported_title, None);
    }

    #[test]
    fn stored_pane_without_manual_title_field_deserializes_for_old_data() {
        let pane: StoredPane = serde_json::from_str(
            r#"{ "left": 0, "top": 0, "width": 80, "height": 24, "buffer": null }"#,
        )
        .expect("old stored pane should deserialize");
        assert_eq!(pane.manual_title, None);

        let roundtrip: StoredPane = serde_json::from_str(
            r#"{ "left": 0, "top": 0, "width": 80, "height": 24, "buffer": null, "manual_title": "api" }"#,
        )
        .expect("new stored pane should deserialize");
        assert_eq!(roundtrip.manual_title.as_deref(), Some("api"));
    }

    #[test]
    fn persisted_native_workspace_parser_accepts_named_layouts() {
        let state = TerminalView::parse_persisted_native_workspace_state(
            r#"{
  "version": 3,
  "last_session": null,
  "layouts": [
    {
      "name": "Main",
      "workspace": {
        "active_tab": 0,
        "tabs": [
          {
            "active_pane": 0,
            "manual_title": null,
            "panes": [
              { "left": 0, "top": 0, "width": 80, "height": 24 }
            ]
          }
        ]
      }
    }
  ]
}"#,
        )
        .expect("workspace state should parse");

        assert!(state.last_session.is_none());
        assert_eq!(state.layouts.len(), 1);
        assert_eq!(state.layouts[0].name, "Main");
        assert_eq!(state.layouts[0].workspace.tabs.len(), 1);
        assert!(!state.layouts[0].workspace.tabs[0].pinned);
    }

    #[test]
    fn persisted_native_workspace_parser_accepts_empty_workspace() {
        let state = TerminalView::parse_persisted_native_workspace_state(
            r#"{
  "version": 3,
  "last_session": {
    "active_tab": 4,
    "tabs": []
  },
  "layouts": []
}"#,
        )
        .expect("workspace state should parse");

        let workspace = state
            .last_session
            .expect("state should include last session");
        assert!(workspace.tabs.is_empty());
        assert_eq!(workspace.active_tab, 0);
    }

    #[test]
    fn persisted_native_workspace_parser_reads_layout_tree() {
        let state = TerminalView::parse_persisted_native_workspace_state(
            r#"{
  "version": 3,
  "last_session": {
    "active_tab": 0,
    "tabs": [
      {
        "active_pane": 1,
        "pinned": false,
        "manual_title": null,
        "layout_tree": {
          "kind": "split",
          "axis": "horizontal",
          "ratio": 0.5,
          "first": { "kind": "leaf", "pane": 0 },
          "second": { "kind": "leaf", "pane": 1 }
        },
        "panes": [
          { "left": 0, "top": 0, "width": 40, "height": 20 },
          { "left": 40, "top": 0, "width": 40, "height": 20 }
        ]
      }
    ]
  },
  "layouts": []
}"#,
        )
        .expect("workspace state should parse");

        let workspace = state
            .last_session
            .expect("state should include last session");
        match workspace.tabs[0]
            .layout_tree
            .as_ref()
            .expect("layout tree should be present")
        {
            PersistedNativeLayoutNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                assert_eq!(*axis, PaneResizeAxis::Horizontal);
                assert_eq!(*ratio, 0.5);
                assert!(matches!(
                    first.as_ref(),
                    PersistedNativeLayoutNode::Leaf { pane: 0 }
                ));
                assert!(matches!(
                    second.as_ref(),
                    PersistedNativeLayoutNode::Leaf { pane: 1 }
                ));
            }
            other => panic!("unexpected layout tree: {other:?}"),
        }
    }

    #[test]
    fn persisted_native_workspace_parser_reads_pinned_tabs() {
        let state = TerminalView::parse_persisted_native_workspace_state(
            r#"{
  "version": 2,
  "last_session": {
    "active_tab": 0,
    "tabs": [
      {
        "active_pane": 0,
        "pinned": true,
        "manual_title": "Pinned",
        "panes": [
          { "left": 0, "top": 0, "width": 80, "height": 24 }
        ]
      }
    ]
  },
  "layouts": []
}"#,
        )
        .expect("workspace state should parse");

        let workspace = state
            .last_session
            .expect("state should include last session");
        assert_eq!(workspace.tabs.len(), 1);
        assert!(workspace.tabs[0].pinned);
    }

    #[test]
    fn persisted_native_workspace_parser_reads_tab_color() {
        let state = TerminalView::parse_persisted_native_workspace_state(
            r#"{
  "version": 2,
  "last_session": {
    "active_tab": 0,
    "tabs": [
      { "active_pane": 0, "color": "blue",
        "panes": [ { "left": 0, "top": 0, "width": 80, "height": 24 } ] },
      { "active_pane": 0,
        "panes": [ { "left": 0, "top": 0, "width": 80, "height": 24 } ] }
    ]
  },
  "layouts": []
}"#,
        )
        .expect("workspace state should parse");

        let tabs = state.last_session.expect("last session").tabs;
        assert_eq!(tabs[0].color.as_deref(), Some("blue"));
        assert_eq!(tabs[1].color, None);
    }

    #[test]
    fn persisted_native_workspace_parser_ignores_removed_pane_fields() {
        let state = TerminalView::parse_persisted_native_workspace_state(
            r#"{
  "version": 3,
  "last_session": {
    "active_tab": 0,
    "tabs": [
      {
        "active_pane": 0,
        "manual_title": "Docs",
        "panes": [
          {
            "kind": "browser",
            "left": 0,
            "top": 0,
            "width": 80,
            "height": 24,
            "browser_url": "https://example.com/docs"
          }
        ]
      }
    ]
  },
  "layouts": []
}"#,
        )
        .expect("workspace state should parse");

        let workspace = state
            .last_session
            .expect("state should include last session");
        assert_eq!(workspace.tabs[0].manual_title.as_deref(), Some("Docs"));
        assert_eq!(workspace.tabs[0].panes[0].width, 80);
    }

    #[test]
    fn persisted_native_workspace_parser_rejects_unknown_version() {
        let error = TerminalView::parse_persisted_native_workspace_state(
            r#"{"version":99,"last_session":null,"layouts":[]}"#,
        )
        .expect_err("unexpected parser success");

        assert!(error.contains("Unsupported workspace state version"));
    }
}

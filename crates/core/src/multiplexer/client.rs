use crate::multiplexer::{discovery, protocol::*};
use crate::{remote::*, *};
use anyhow::{Context, bail, ensure};
use flume::Sender;
use std::{
    net::{Shutdown, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

struct Connection {
    stream: Mutex<TcpStream>,
    conditional_layout_updates: bool,
}

impl Connection {
    fn connect(root: &Path) -> anyhow::Result<Self> {
        let (stream, capabilities) = discovery::connect_with_capabilities(root)?;
        Ok(Self {
            stream: Mutex::new(stream),
            conditional_layout_updates: capabilities.conditional_layout_updates,
        })
    }
    fn request(&self, request: Request) -> anyhow::Result<Response> {
        let mut stream = self.stream.lock().unwrap();
        write_message(&mut *stream, &request)?;
        match read_message(&mut *stream)? {
            Response::Error(error) => bail!(error),
            response => Ok(response),
        }
    }
}

#[derive(Clone)]
pub struct SessionClient {
    root: PathBuf,
    connection: Arc<Connection>,
}

impl SessionClient {
    /// Apply a workspace edit without replacing another client's layout edits.
    pub fn edit_workspace(
        &self,
        window_id: &str,
        index: usize,
        expected: &crate::session_model::StoredWorkspace,
        edit: &crate::session_model::WorkspaceEdit,
    ) -> anyhow::Result<crate::session_model::StoredMultiplexer> {
        for _ in 0..8 {
            let serialized = self
                .layout()?
                .ok_or_else(|| anyhow::anyhow!("No saved session layout"))?;
            let mut layout: crate::session_model::StoredMultiplexer =
                serde_json::from_str(&serialized)?;
            ensure!(layout.version == 1, "Unsupported session layout version");
            let workspace = layout
                .windows
                .iter_mut()
                .find(|window| window.id == window_id)
                .and_then(|window| window.session.workspaces.get_mut(index))
                .ok_or_else(|| anyhow::anyhow!("Workspace no longer exists"))?;
            ensure!(
                workspace == expected,
                "Workspace changed in another client; refresh before editing"
            );
            edit.apply(workspace)
                .map_err(|error| anyhow::anyhow!(error))?;
            let replacement = serde_json::to_string(&layout)?;
            if self.compare_and_set_layout(Some(serialized), replacement)? {
                return Ok(layout);
            }
        }
        bail!("Session layout is changing; retry the edit")
    }
    pub fn supports_conditional_layout_updates(&self) -> bool {
        self.connection.conditional_layout_updates
    }
    pub fn connect(root: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            root: root.to_owned(),
            connection: Arc::new(Connection::connect(root)?),
        })
    }

    pub fn create(
        &self,
        launch: PaneLaunch,
        wakeup: Option<TerminalWakeupNotifier>,
    ) -> anyhow::Result<(String, Terminal)> {
        let Response::Attached(info, state) =
            self.connection.request(Request::Create(Box::new(launch)))?
        else {
            bail!("invalid create response");
        };
        let id = info.id;
        let terminal = self.attach_state(&id, state, wakeup)?;
        Ok((id, terminal))
    }

    pub fn attach(
        &self,
        id: &str,
        wakeup: Option<TerminalWakeupNotifier>,
    ) -> anyhow::Result<Terminal> {
        let Response::Attached(_, state) =
            self.connection.request(Request::Attach(id.to_owned()))?
        else {
            bail!("invalid attach response");
        };
        self.attach_state(id, state, wakeup)
    }

    fn attach_state(
        &self,
        id: &str,
        state: Arc<RemoteState>,
        wakeup: Option<TerminalWakeupNotifier>,
    ) -> anyhow::Result<Terminal> {
        validate_state(&state)?;
        let mut initial_state = (*state).clone();
        // Painting reads a completed local snapshot, even before the first
        // graphics update arrives. It must never make a synchronous image RPC.
        initial_state.graphics = Some(Vec::new());
        let shared = Arc::new(ClientState {
            state: Mutex::new(Arc::new(initial_state)),
            state_updated: Condvar::new(),
            pending: Mutex::new(Pending::default()),
            wakeup,
            wakeup_enabled: AtomicBool::new(true),
            disconnected: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        });
        let link = open_link(&self.root, id, &shared, 0, false)?;
        Ok(Terminal::from_remote(Arc::new(ClientTerminal {
            id: id.to_owned(),
            root: self.root.clone(),
            host: Arc::clone(&self.connection),
            shared,
            link: Mutex::new(link),
        })))
    }

    pub fn list(&self) -> anyhow::Result<Vec<PaneInfo>> {
        match self.connection.request(Request::List)? {
            Response::Panes(panes) => Ok(panes),
            _ => bail!("invalid session list"),
        }
    }
    pub fn close(&self, id: &str) -> anyhow::Result<()> {
        self.connection.request(Request::Close(id.to_owned()))?;
        Ok(())
    }
    /// Wait for the host to accept input before returning. Unlike a view's
    /// asynchronous input queue, this is safe for a short-lived CLI process.
    pub fn write(&self, id: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.send_command(id, RemoteCommand::Write(bytes))
    }
    pub fn resize(&self, id: &str, size: TerminalSize) -> anyhow::Result<()> {
        self.send_command(id, RemoteCommand::Resize(size))
    }
    fn send_command(&self, id: &str, command: RemoteCommand) -> anyhow::Result<()> {
        match self.connection.request(Request::Command {
            pane: id.to_owned(),
            command,
        })? {
            Response::Reply(_) => Ok(()),
            _ => bail!("invalid terminal command response"),
        }
    }
    pub fn layout(&self) -> anyhow::Result<Option<String>> {
        match self.connection.request(Request::GetLayout)? {
            Response::Layout(layout) => Ok(layout),
            _ => bail!("invalid session layout"),
        }
    }
    pub fn set_layout(&self, layout: String) -> anyhow::Result<()> {
        self.connection.request(Request::SetLayout(layout))?;
        Ok(())
    }
    /// Publish a layout only if no other client changed the snapshot we read.
    /// A false result leaves the host's layout untouched.
    pub fn compare_and_set_layout(
        &self,
        expected: Option<String>,
        replacement: String,
    ) -> anyhow::Result<bool> {
        ensure!(
            self.supports_conditional_layout_updates(),
            "This running multiplexer predates CLI layout editing; keep its terminals attached or restart it after closing its sessions to enable layout editing"
        );
        match self.connection.request(Request::CompareAndSetLayout {
            expected,
            replacement,
        })? {
            Response::LayoutUpdated(updated) => Ok(updated),
            _ => bail!("multiplexer host does not support conditional layout updates"),
        }
    }
    /// Explicitly terminate all hosted terminals and the background host.
    pub fn shutdown(&self) -> anyhow::Result<()> {
        self.connection.request(Request::Shutdown)?;
        Ok(())
    }
}

fn validate_state(state: &RemoteState) -> anyhow::Result<()> {
    let metadata = state.render.metadata;
    ensure!(
        metadata.cols > 0
            && metadata.rows > 0
            && metadata.cols == state.size.cols
            && metadata.rows == state.size.rows,
        "invalid remote terminal dimensions"
    );
    ensure!(
        state.render.cells.len() == usize::from(metadata.cols) * usize::from(metadata.rows),
        "incomplete remote terminal viewport"
    );
    Ok(())
}

#[derive(Default)]
struct Pending {
    wakeup: bool,
    events: Vec<TerminalEvent>,
    hosts: Vec<(u64, RemoteHostRequest)>,
}

/// 一条订阅链路：RPC 连接、输入队列与订阅流；重新订阅时整体替换。
struct Link {
    /// 本链路的代数；只有与 `ClientState::generation` 一致的链路才能标记断连。
    generation: u64,
    rpc: Arc<Connection>,
    input: Sender<RemoteCommand>,
    close_stream: TcpStream,
}

/// 建立一条订阅链路并启动读、输入两个线程。
///
/// 握手成功后才把 `shared.generation` 切到 `generation`，此后旧链路线程的失败与迟到的更新都会被忽略。
/// `reset_pending` 为 `true`（重新订阅）时同时清掉本地待处理事件并复位断连标记，
/// 因为宿主会在新订阅里重发粘性事件与最新屏幕，旧队列只会造成重复或乱序。
fn open_link(
    root: &Path,
    id: &str,
    shared: &Arc<ClientState>,
    generation: u64,
    reset_pending: bool,
) -> anyhow::Result<Link> {
    let rpc = Connection::connect(root)?;
    let input = Connection::connect(root)?;
    let (mut stream, capabilities) = discovery::connect_with_capabilities(root)?;
    let subscription = if capabilities.graphics_stream {
        Request::SubscribeGraphics(id.to_owned())
    } else {
        Request::Subscribe(id.to_owned())
    };
    write_message(&mut stream, &subscription)?;
    ensure!(
        matches!(read_message(&mut stream)?, Response::Ok),
        "cannot subscribe to terminal session"
    );
    stream.set_read_timeout(None)?;
    let close_stream = stream.try_clone()?;
    let (input_tx, input_rx) = flume::bounded(256);
    let legacy_graphics = if capabilities.graphics_stream {
        None
    } else {
        Some(Connection::connect(root)?)
    };
    shared.generation.store(generation, Ordering::Release);
    if reset_pending {
        let mut pending = shared.pending.lock().unwrap();
        pending.events.clear();
        pending.hosts.clear();
        shared.disconnected.store(false, Ordering::Release);
    }
    let reader_shared = Arc::clone(shared);
    let reader_pane_id = id.to_owned();
    std::thread::Builder::new()
        .name("termy-session-read".into())
        .spawn(move || {
            let mut graphics = graphics::GraphicsDecoder::default();
            let mut legacy_cache = LegacyGraphicsCache::default();
            let result = (|| -> anyhow::Result<()> {
                loop {
                    let update = read_message(&mut stream)?;
                    // 已被新链路取代：丢弃迟到的更新，静默退出。
                    if reader_shared.generation.load(Ordering::Acquire) != generation {
                        return Ok(());
                    }
                    match update {
                        Update::State(mut state) => {
                            validate_state(&state)?;
                            let connection = legacy_graphics
                                .as_ref()
                                .context("unexpected legacy terminal update")?;
                            legacy_cache.update(Arc::make_mut(&mut state), || {
                                match connection.request(Request::Command {
                                    pane: reader_pane_id.clone(),
                                    command: RemoteCommand::Graphics,
                                })? {
                                    Response::Reply(RemoteReply::Graphics(revision, placements)) => {
                                        Ok((revision, placements))
                                    }
                                    _ => bail!("invalid terminal graphics response"),
                                }
                            })?;
                            reader_shared.publish_state(state);
                        }
                        Update::GraphicsState(mut state, update) => {
                            validate_state(&state)?;
                            Arc::make_mut(&mut state).graphics = Some(graphics.decode(update)?);
                            reader_shared.publish_state(state);
                        }
                        Update::Events(events) => {
                            let mut pending = reader_shared.pending.lock().unwrap();
                            push_pending_events(&mut pending.events, events);
                        }
                        Update::Host { id, request } => reader_shared
                            .pending
                            .lock()
                            .unwrap()
                            .hosts
                            .push((id, request)),
                    }
                    reader_shared.notify();
                }
            })();
            if let Err(error) = result {
                reader_shared.fail_link(generation, &error);
            }
        })?;
    let input_shared = Arc::clone(shared);
    let pane_id = id.to_owned();
    std::thread::Builder::new()
        .name("termy-session-input".into())
        .spawn(move || {
            while let Ok(command) = input_rx.recv() {
                if let Err(error) = input.request(Request::Command {
                    pane: pane_id.clone(),
                    command,
                }) {
                    input_shared.fail_link(generation, &error);
                    break;
                }
            }
        })?;
    Ok(Link {
        generation,
        rpc: Arc::new(rpc),
        input: input_tx,
        close_stream,
    })
}

/// 客户端待消费事件队列的上限；后台窗格没人取事件时靠它兜底，防止无限增长。
const MAX_PENDING_EVENTS: usize = 1024;

/// 把宿主推来的事件并入待消费队列，永不因积压而失败。
///
/// - 粘性事件（标题、cwd、进度等）只保留每组最新一条；
/// - 超过 [`MAX_PENDING_EVENTS`] 时丢弃最旧的非 `Exit` 事件，`Exit` 始终保留。
///
/// 后台标签的窗格会关闭唤醒、无人取事件，而 claude 会持续刷新标题；
/// 以前这里溢出即报错，读线程退出后窗格永久断连。
fn push_pending_events(queue: &mut Vec<TerminalEvent>, incoming: Vec<TerminalEvent>) {
    for event in incoming {
        if let Some(key) = super::pane::sticky_key(&event) {
            queue.retain(|old| super::pane::sticky_key(old) != Some(key));
        }
        queue.push(event);
    }
    while queue.len() > MAX_PENDING_EVENTS {
        let Some(index) = queue
            .iter()
            .position(|event| !matches!(event, TerminalEvent::Exit))
        else {
            break;
        };
        queue.remove(index);
    }
}

struct ClientState {
    state: Mutex<Arc<RemoteState>>,
    state_updated: Condvar,
    pending: Mutex<Pending>,
    wakeup: Option<TerminalWakeupNotifier>,
    wakeup_enabled: AtomicBool,
    disconnected: AtomicBool,
    closing: AtomicBool,
    /// 当前有效链路的代数；重新订阅时递增，旧链路的失败据此被忽略。
    generation: AtomicU64,
}

impl ClientState {
    fn publish_state(&self, state: Arc<RemoteState>) {
        *self.state.lock().unwrap() = state;
        self.pending.lock().unwrap().wakeup = true;
        self.state_updated.notify_all();
    }

    fn wait_for_generation(&self, generation: u64) -> anyhow::Result<()> {
        let (state, _) = self
            .state_updated
            .wait_timeout_while(
                self.state.lock().unwrap(),
                Duration::from_secs(5),
                |state| {
                    state.render.metadata.generation < generation
                        && !self.disconnected.load(Ordering::Acquire)
                },
            )
            .unwrap();
        ensure!(
            !self.disconnected.load(Ordering::Acquire),
            "terminal session disconnected"
        );
        ensure!(
            state.render.metadata.generation >= generation,
            "terminal viewport did not catch up"
        );
        Ok(())
    }

    fn notify(&self) {
        if self.wakeup_enabled.load(Ordering::Acquire)
            && let Some(wakeup) = &self.wakeup
        {
            wakeup.notify();
        }
    }
    /// 仅当 `generation` 仍是当前链路时才标记断连；被新链路取代的旧链路出错不算数。
    fn fail_link(&self, generation: u64, error: &anyhow::Error) {
        if self.generation.load(Ordering::Acquire) == generation {
            self.fail(error);
        }
    }
    fn fail(&self, error: &anyhow::Error) {
        if self.closing.load(Ordering::Acquire) || self.disconnected.swap(true, Ordering::AcqRel) {
            return;
        }
        log::error!("terminal session connection lost: {error}");
        // Pair the notification with the state mutex used by scroll waiters.
        let state = self.state.lock().unwrap();
        self.state_updated.notify_all();
        drop(state);
        self.pending
            .lock()
            .unwrap()
            .events
            .push(TerminalEvent::Title(
                "Session disconnected — reopen Termarx to reconnect".into(),
            ));
        self.notify();
    }
}

struct ClientTerminal {
    id: String,
    root: PathBuf,
    host: Arc<Connection>,
    shared: Arc<ClientState>,
    /// 当前订阅链路；`reconnect` 时整体替换。
    link: Mutex<Link>,
}

impl Drop for ClientTerminal {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        if let Ok(link) = self.link.get_mut() {
            let _ = link.close_stream.shutdown(Shutdown::Both);
        }
    }
}

impl RemoteTransport for ClientTerminal {
    fn state(&self) -> Arc<RemoteState> {
        Arc::clone(&self.shared.state.lock().unwrap())
    }
    fn request(&self, command: RemoteCommand) -> anyhow::Result<RemoteReply> {
        let changes_viewport = matches!(
            command,
            RemoteCommand::Scroll(_)
                | RemoteCommand::ScrollToBottom
                | RemoteCommand::ClearScrollback
        );
        let (rpc, generation) = {
            let link = self.link.lock().unwrap();
            (Arc::clone(&link.rpc), link.generation)
        };
        match rpc.request(Request::Command {
            pane: self.id.clone(),
            command,
        }) {
            Ok(Response::Reply(reply)) => {
                if changes_viewport && matches!(reply, RemoteReply::Changed(true)) {
                    // Attach refreshes/publishes immediately on every supported
                    // host. Wait for that generation on the ordered subscription
                    // (including its graphics), rather than letting selection
                    // record the old offset during the 16ms frame interval.
                    let result = (|| {
                        let Response::Attached(_, state) =
                            rpc.request(Request::Attach(self.id.clone()))?
                        else {
                            bail!("invalid terminal refresh response");
                        };
                        self.shared
                            .wait_for_generation(state.render.metadata.generation)
                    })();
                    if let Err(error) = result {
                        self.shared.fail_link(generation, &error);
                        return Err(error);
                    }
                }
                Ok(reply)
            }
            Ok(_) => bail!("invalid terminal response"),
            Err(error) => {
                self.shared.fail_link(generation, &error);
                Err(error)
            }
        }
    }
    fn send(&self, command: RemoteCommand) {
        if self.shared.disconnected.load(Ordering::Acquire) {
            return;
        }
        let (input, generation) = {
            let link = self.link.lock().unwrap();
            (link.input.clone(), link.generation)
        };
        if let Err(error) = input.send_timeout(command, Duration::from_millis(100)) {
            self.shared
                .fail_link(generation, &anyhow::anyhow!(error.to_string()));
        }
    }
    fn take_events(&self) -> Vec<TerminalEvent> {
        let mut pending = self.shared.pending.lock().unwrap();
        let mut events = std::mem::take(&mut pending.events);
        if std::mem::take(&mut pending.wakeup) {
            events.push(TerminalEvent::Wakeup);
        }
        events
    }
    fn take_host_requests(&self) -> Vec<(u64, RemoteHostRequest)> {
        std::mem::take(&mut self.shared.pending.lock().unwrap().hosts)
    }
    fn reply_to_host(&self, id: u64, reply: RemoteHostReply) {
        if let Err(error) = self.host.request(Request::HostReply {
            pane: self.id.clone(),
            id,
            reply,
        }) {
            self.shared.fail(&error);
        }
    }
    fn has_pending_events(&self) -> bool {
        let pending = self.shared.pending.lock().unwrap();
        pending.wakeup || !pending.events.is_empty() || !pending.hosts.is_empty()
    }
    fn set_wakeup_enabled(&self, enabled: bool) {
        self.shared.wakeup_enabled.store(enabled, Ordering::Release);
        if enabled && self.has_pending_events() {
            self.shared.notify();
        }
    }
    fn reconnect(&self) -> anyhow::Result<()> {
        let generation = self.shared.generation.load(Ordering::Acquire) + 1;
        // 新链路握手失败时旧链路原样保留，调用方可以提示用户改走"强制结束任务"。
        let link = open_link(&self.root, &self.id, &self.shared, generation, true)?;
        let old = std::mem::replace(&mut *self.link.lock().unwrap(), link);
        let _ = old.close_stream.shutdown(Shutdown::Both);
        self.shared.pending.lock().unwrap().wakeup = true;
        self.shared.notify();
        Ok(())
    }
    fn is_disconnected(&self) -> bool {
        self.shared.disconnected.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticky_events_keep_only_the_latest_per_group() {
        let mut queue = Vec::new();
        let titles = (0..5000).map(|i| TerminalEvent::Title(format!("spin {i}")));
        push_pending_events(&mut queue, titles.collect());
        assert!(matches!(queue.as_slice(), [TerminalEvent::Title(t)] if t == "spin 4999"));
    }

    /// 构造一个不带唤醒回调的最小 `ClientState`，用于验证断连代数逻辑。
    fn bare_state(generation: u64) -> ClientState {
        let terminal = Terminal::new_display(TerminalSize::default(), None);
        ClientState {
            state: Mutex::new(Arc::new(RemoteState::capture(&terminal))),
            state_updated: Condvar::new(),
            pending: Mutex::new(Pending::default()),
            wakeup: None,
            wakeup_enabled: AtomicBool::new(true),
            disconnected: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            generation: AtomicU64::new(generation),
        }
    }

    #[test]
    fn stale_link_failure_does_not_disconnect_the_new_link() {
        let state = bare_state(2);
        state.fail_link(1, &anyhow::anyhow!("old link closed"));
        assert!(!state.disconnected.load(Ordering::Acquire));
        state.fail_link(2, &anyhow::anyhow!("current link lost"));
        assert!(state.disconnected.load(Ordering::Acquire));
    }

    #[test]
    fn overflow_drops_oldest_events_instead_of_failing() {
        let mut queue = Vec::new();
        push_pending_events(&mut queue, vec![TerminalEvent::Exit]);
        push_pending_events(&mut queue, vec![TerminalEvent::Bell; MAX_PENDING_EVENTS + 10]);
        assert_eq!(queue.len(), MAX_PENDING_EVENTS);
        assert!(queue.iter().any(|e| matches!(e, TerminalEvent::Exit)));
    }
}

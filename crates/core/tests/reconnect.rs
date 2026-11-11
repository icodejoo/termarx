//! 验证窗格"重新订阅"：经真实会话宿主建立窗格后重连，屏幕内容要被宿主重发，
//! 且重连后输入仍然能到达子进程。仅 Windows 运行（用 cmd.exe 作为窗格里的程序）。
#![cfg(windows)]

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use termy_core::multiplexer::{PaneLaunch, SessionClient, connect_or_start};
use termy_core::*;

/// 测试结束时关闭宿主，避免遗留常驻进程。
struct Host {
    client: SessionClient,
    _temp: tempfile::TempDir,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.client.shutdown();
    }
}

/// 取窗格当前屏幕上的全部文本。
fn screen_text(terminal: &Terminal) -> String {
    let render = terminal.render_read(true);
    render
        .cells
        .iter()
        .filter(|cell| !cell.wide_character_spacer)
        .map(|cell| if cell.hidden { " " } else { cell.text.as_str() })
        .collect()
}

/// 等屏幕上出现 `needle`，超时则报错。
#[track_caller]
fn wait_for_text(terminal: &Terminal, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !screen_text(terminal).contains(needle) {
        assert!(
            Instant::now() < deadline,
            "screen never showed {needle:?}: {}",
            screen_text(terminal)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn reconnect_restores_screen_and_input() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("host");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_termarx-session-host"));
    let client = connect_or_start(&root, &executable).expect("start host");
    let host = Host {
        client,
        _temp: temp,
    };
    let (_, terminal) = host
        .client
        .create(
            PaneLaunch {
                size: TerminalSize {
                    cols: 100,
                    rows: 24,
                    ..TerminalSize::default()
                },
                working_directory: None,
                shell_integration: None,
                config: TerminalRuntimeConfig::default(),
                launch: Some(TerminalLaunch::Program {
                    program: "cmd.exe".into(),
                    args: Vec::new(),
                }),
            },
            None,
        )
        .expect("create pane");

    terminal.write(b"echo BEFORE-RECONNECT\r\n");
    wait_for_text(&terminal, "BEFORE-RECONNECT");
    assert!(!terminal.is_disconnected());

    terminal.reconnect().expect("reconnect");
    assert!(!terminal.is_disconnected());

    // 宿主重发的屏幕里还有重连前的内容，且新输入能到达子进程
    wait_for_text(&terminal, "BEFORE-RECONNECT");
    terminal.write(b"echo AFTER-RECONNECT\r\n");
    wait_for_text(&terminal, "AFTER-RECONNECT");

    // 本地终端无订阅链路，重连恒成功、恒不算断开
    let local = Terminal::new_display(TerminalSize::default(), None);
    assert!(local.reconnect().is_ok());
    assert!(!local.is_disconnected());
}

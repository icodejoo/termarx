//! 真实 PTY 下验证：TUI 开了备用屏幕和鼠标上报后被杀，shell 提示符出现在备用屏幕里时，
//! 终端能感知到输入上报状态残留（鼠标一动就往 shell 里输入坐标的根源）。

use std::time::{Duration, Instant};
use termy_core::*;

/// 等条件成立，超时则 panic。
#[track_caller]
fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 生成 PTY 里运行的脚本：进入备用屏幕 + 开鼠标上报 + SGR 编码，随后输出提示符标记
/// （OSC 133;A）并保持存活，返回启动命令。写成脚本文件是为了绕开 shell 对转义字符的二次解析。
#[cfg(windows)]
fn startup_command(dir: &std::path::Path) -> String {
    let script = dir.join("termarx_stale_tui.ps1");
    let body = [
        "$e=[char]27",
        "[Console]::Out.Write(\"$e[?1049h$e[?1003h$e[?1006h\")",
        "Start-Sleep -Milliseconds 800",
        "[Console]::Out.Write(\"$e]133;A$e$([char]92)PROMPT> \")",
        "Start-Sleep -Seconds 30",
    ]
    .join("\r\n");
    std::fs::write(&script, body).unwrap();
    format!(
        "powershell -NoProfile -ExecutionPolicy Bypass -File {}",
        script.display()
    )
}

/// 同上，Unix 版用 printf 输出。
#[cfg(not(windows))]
fn startup_command(_dir: &std::path::Path) -> String {
    "printf '\033[?1049h\033[?1003h\033[?1006h'; sleep 0.5; printf '\033]133;A\033\\PROMPT> '; sleep 30".into()
}

#[test]
fn dead_tui_prompt_in_alternate_screen_is_detected_as_stale() {
    let startup = startup_command(&std::env::temp_dir());
    let terminal = Terminal::new(
        TerminalSize {
            cols: 60,
            rows: 8,
            ..TerminalSize::default()
        },
        None,
        None,
        None,
        None,
        Some(startup.as_str()),
    )
    .expect("spawn terminal");

    // 先进入 TUI 状态：备用屏幕 + 鼠标上报开启
    wait_until(|| terminal.alternate_screen_mode() && terminal.mouse_mode().enabled);
    // 提示符标记到达后判定为残留
    wait_until(|| terminal.has_stale_input_report_modes());
    assert!(terminal.alternate_screen_mode());
    assert!(terminal.prompt_in_alt_screen());

    // 硬复位后：退出备用屏幕、关闭鼠标上报、标记清除
    terminal.reset_input_modes(true);
    assert!(!terminal.alternate_screen_mode());
    assert!(!terminal.mouse_mode().enabled);
    assert!(!terminal.has_stale_input_report_modes());
}

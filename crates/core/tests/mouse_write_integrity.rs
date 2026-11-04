//! 验证 SGR 鼠标报告经 ConPTY 写给子进程（node 原始模式读 stdin，和 Claude Code 同类）的完整性：
//! 字节流必须原样到达；数据块边界落在报告内部的次数用来衡量"被拆开"的风险。
//!
//! 结论（本机实测）：字节流始终一致；无间隔猛写时子进程的读取会在报告中间截断（积压导致），
//! 每 25ms 一条（应用的节流速率）时在空闲机器上不会截断（负载高时仍可能出现，所以不做断言）。所以降低上报速率是有效的缓解手段。
//!
//! 环境变量 `TERMARX_TEST_CONPTY=bundled` 时，把 `vendor/conpty` 里的内置 ConPTY 复制到测试
//! 可执行文件旁（和安装包一致）；不设置则用系统自带的 ConPTY。仅 Windows、且装有 node 时运行。
#![cfg(windows)]

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use termy_core::*;

/// 要写入的鼠标报告条数。

/// 子进程脚本：原始模式读 stdin，每个数据块记一行十六进制；同时周期性占用事件循环，模拟 TUI 繁忙。
const CHILD_SCRIPT: &str = r#"
const fs = require('fs');
const out = process.argv[2];
const expected = Number(process.argv[3]);
process.stdin.setRawMode(true);
process.stdin.resume();
let total = 0;
fs.writeFileSync(out, '');
process.stdin.on('data', (chunk) => {
  fs.appendFileSync(out, chunk.toString('hex') + '\n');
  total += chunk.length;
  if (total >= expected) { fs.appendFileSync(out, 'DONE\n'); }
});
setInterval(() => { const end = Date.now() + 30; while (Date.now() < end) {} }, 60);
setTimeout(() => process.exit(0), 60000);
"#;

/// 工作目录：临时目录下的专用子目录（路径里不能有空格，避免 shell 二次解析）。
fn work_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("termarx_mouse_integrity");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 把内置 ConPTY 复制到测试可执行文件旁。
fn install_bundled_conpty() {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/conpty")
        .join(arch);
    let target = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    for name in ["conpty.dll", "OpenConsole.exe"] {
        // 同一进程里第二个测试再复制会因为 dll 已被加载而失败，已存在就跳过
        if !target.join(name).exists() {
            std::fs::copy(source.join(name), target.join(name)).expect("copy bundled conpty");
        }
    }
}

/// 两个测试都占满 CPU 和 PTY，并行会互相干扰，用锁串行执行。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 一次运行的统计：数据块总数、边界落在报告内部的块数、字节流是否和写入完全一致。
struct Stats {
    chunks: usize,
    split_chunks: usize,
    identical: bool,
}

/// 写 `count` 条报告，每条之间间隔 `interval`，返回子进程收到的统计。`tag` 区分日志文件。
fn run(count: usize, interval: Duration, tag: &str) -> Option<Stats> {
    if Command::new("node").arg("-v").output().is_err() {
        eprintln!("node not found, skipping");
        return None;
    }
    let bundled = std::env::var("TERMARX_TEST_CONPTY").is_ok_and(|value| value == "bundled");
    if bundled {
        install_bundled_conpty();
    }

    let dir = work_dir();
    let script = dir.join("child.js");
    let log = dir.join(format!("chunks_{tag}.log"));
    std::fs::write(&script, CHILD_SCRIPT).unwrap();

    // 预先生成所有报告，记下总字节数
    let packets: Vec<Vec<u8>> = (0..count)
        .map(|i| format!("\x1b[<35;{};{}M", 1 + i % 200, 1 + (i / 7) % 50).into_bytes())
        .collect();
    let expected: usize = packets.iter().map(Vec::len).sum();

    // 清掉上次遗留的日志，否则下面会把旧文件当成"子进程已就绪"
    let _ = std::fs::remove_file(&log);
    let command = format!("node {} {} {}", script.display(), log.display(), expected);
    let size = TerminalSize {
        cols: 80,
        rows: 24,
        ..TerminalSize::default()
    };
    // TERMARX_TEST_REMOTE=1：走应用实际使用的 session host（客户端 -> IPC -> 宿主进程 -> PTY）
    let remote = std::env::var("TERMARX_TEST_REMOTE").is_ok();
    let terminal = if remote {
        let host = PathBuf::from(env!("CARGO_BIN_EXE_termarx-session-host"));
        if bundled {
            let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/conpty/x64");
            for name in ["conpty.dll", "OpenConsole.exe"] {
                let target = host.parent().unwrap().join(name);
                if !target.exists() {
                    std::fs::copy(source.join(name), target).expect("copy bundled conpty to host");
                }
            }
        }
        let root = dir.join(format!("mux_{tag}"));
        let client = termy_core::multiplexer::connect_or_start(&root, &host).expect("start host");
        let launch = termy_core::multiplexer::PaneLaunch {
            size,
            working_directory: None,
            shell_integration: None,
            config: TerminalRuntimeConfig::default(),
            launch: Some(TerminalLaunch::Program {
                program: "node".into(),
                args: vec![
                    script.display().to_string(),
                    log.display().to_string(),
                    expected.to_string(),
                ],
            }),
        };
        client.create(launch, None).expect("create remote pane").1
    } else {
        Terminal::new(size, None, None, None, None, Some(command.as_str())).expect("spawn terminal")
    };

    // 等子进程把日志文件建出来（说明已进入原始模式开始读）
    let deadline = Instant::now() + Duration::from_secs(20);
    while !log.exists() {
        assert!(Instant::now() < deadline, "child did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(800));

    // 和应用一样，每个报告一次 write，不加间隔，最坏情况的高频写入
    for packet in &packets {
        terminal.write(packet);
        if !interval.is_zero() {
            std::thread::sleep(interval);
        }
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    let content = loop {
        let content = std::fs::read_to_string(&log).unwrap_or_default();
        if content.contains("DONE") || Instant::now() > deadline {
            break content;
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    // 把所有数据块还原成字节流，同时统计"数据块边界落在报告内部"的次数
    let mut received = Vec::new();
    let mut split_boundaries = 0usize;
    let mut chunks = 0usize;
    for line in content
        .lines()
        .filter(|line| *line != "DONE" && !line.is_empty())
    {
        let bytes: Vec<u8> = (0..line.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap())
            .collect();
        chunks += 1;
        // 完整的数据块应以 ESC 开头、以 M 结尾
        if bytes.first() != Some(&0x1b) || bytes.last() != Some(&b'M') {
            split_boundaries += 1;
        }
        received.extend(bytes);
    }
    let sent: Vec<u8> = packets.concat();
    if sent != received {
        let mismatch = sent.iter().zip(&received).position(|(a, b)| a != b);
        eprintln!(
            "MISMATCH sent={} received={} first_diff={mismatch:?} done_marker={}",
            sent.len(),
            received.len(),
            content.contains("DONE")
        );
    }
    Some(Stats {
        chunks,
        split_chunks: split_boundaries,
        identical: sent == received,
    })
}

/// 当前使用的 ConPTY 名称（仅用于输出）。
fn conpty_label() -> &'static str {
    if std::env::var("TERMARX_TEST_CONPTY").is_ok_and(|value| value == "bundled") {
        "bundled"
    } else {
        "system"
    }
}

#[test]
fn burst_write_keeps_the_byte_stream_intact() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(stats) = run(
        std::env::var("TERMARX_TEST_COUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20000),
        Duration::ZERO,
        &format!("burst_{}", conpty_label()),
    ) else {
        return;
    };
    eprintln!(
        "burst conpty={} chunks={} split_chunks={} identical={}",
        conpty_label(),
        stats.chunks,
        stats.split_chunks,
        stats.identical
    );
    assert!(stats.identical, "收到的字节和写入的不一致（丢失或被改写）");
}

#[test]
fn throttled_rate_keeps_the_byte_stream_intact() {
    // 25ms 一条 = 应用移动报告节流后的最高速率
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(stats) = run(
        300,
        Duration::from_millis(25),
        &format!("rate_{}", conpty_label()),
    ) else {
        return;
    };
    eprintln!(
        "throttled conpty={} chunks={} split_chunks={} identical={}",
        conpty_label(),
        stats.chunks,
        stats.split_chunks,
        stats.identical
    );
    assert!(stats.identical);
    // 截断次数取决于机器负载（并行跑其他测试时会出现），只输出不断言
}

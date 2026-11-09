//! “终止全部任务并退出”的进程清理：杀掉本程序启动的全部进程树。
//!
//! 范围：当前进程的所有后代，加上同名的其他实例（例如常驻的会话 host、
//! 其他窗口）及它们的后代。当前进程本身不会被杀，由调用方正常退出。

use std::collections::{HashMap, HashSet};

/// 进程快照中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcEntry {
    /// 进程 ID。
    pub pid: u32,
    /// 父进程 ID。
    pub ppid: u32,
    /// 可执行文件名（不含目录）。
    pub name: String,
}

/// 计算需要终止的进程 ID（叶子优先，不含 `self_pid`）。
///
/// - `procs`：当前系统的进程快照
/// - `self_pid` / `self_name`：当前进程的 ID 与可执行文件名；同名进程视为同一应用的其他实例
///
/// 返回值按“子孙在前、祖先在后”排序，便于逐个终止。
pub(crate) fn kill_set(procs: &[ProcEntry], self_pid: u32, self_name: &str) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs {
        // 自己指向自己或 ppid 为 0 的项没有意义，跳过以免成环。
        if proc.pid != proc.ppid && proc.ppid != 0 {
            children.entry(proc.ppid).or_default().push(proc.pid);
        }
    }

    let mut roots = vec![self_pid];
    roots.extend(
        procs
            .iter()
            .filter(|proc| proc.pid != self_pid && proc.name.eq_ignore_ascii_case(self_name))
            .map(|proc| proc.pid),
    );

    let mut visited: HashSet<u32> = HashSet::from([self_pid]);
    let mut order = Vec::new();
    let mut queue: std::collections::VecDeque<u32> = roots.into_iter().collect();
    while let Some(pid) = queue.pop_front() {
        if pid != self_pid && !visited.insert(pid) {
            continue;
        }
        if pid != self_pid {
            order.push(pid);
        }
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().copied().filter(|kid| !visited.contains(kid)));
        }
    }
    // 广度优先的逆序即“子孙在前”。
    order.reverse();
    order
}

/// 终止当前进程之外、本程序启动的全部进程，返回成功终止的数量。
///
/// 使用示例：退出前调用 `process_kill::kill_all_other_processes()`，再正常退出应用。
pub(crate) fn kill_all_other_processes() -> usize {
    let Ok(exe) = std::env::current_exe() else {
        return 0;
    };
    let Some(self_name) = exe.file_name().and_then(|name| name.to_str()) else {
        return 0;
    };
    let procs = platform::snapshot();
    let targets = kill_set(&procs, std::process::id(), self_name);
    targets
        .into_iter()
        .filter(|pid| platform::terminate(*pid))
        .count()
}

#[cfg(target_os = "windows")]
mod platform {
    use super::ProcEntry;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    /// 枚举当前系统的全部进程。
    pub(super) fn snapshot() -> Vec<ProcEntry> {
        let mut procs = Vec::new();
        // SAFETY: 快照句柄在函数内创建并关闭；entry 的 dwSize 已按 API 要求初始化。
        unsafe {
            let Ok(handle) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return procs;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut ok = Process32FirstW(handle, &mut entry).is_ok();
            while ok {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                procs.push(ProcEntry {
                    pid: entry.th32ProcessID,
                    ppid: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&entry.szExeFile[..len]),
                });
                ok = Process32NextW(handle, &mut entry).is_ok();
            }
            let _ = CloseHandle(handle);
        }
        procs
    }

    /// 强制终止指定进程，返回是否成功。
    pub(super) fn terminate(pid: u32) -> bool {
        // SAFETY: 句柄仅在本函数内使用并关闭。
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) else {
                return false;
            };
            let killed = TerminateProcess(handle, 1).is_ok();
            let _ = CloseHandle(handle);
            killed
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::ProcEntry;
    use std::process::Command;

    /// 通过 `ps` 枚举当前系统的全部进程。
    pub(super) fn snapshot() -> Vec<ProcEntry> {
        let Ok(output) = Command::new("ps")
            .args(["-axo", "pid=,ppid=,comm="])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let pid = parts.next()?.parse().ok()?;
                let ppid = parts.next()?.parse().ok()?;
                let comm = parts.collect::<Vec<_>>().join(" ");
                let name = comm.rsplit('/').next()?.to_string();
                Some(ProcEntry { pid, ppid, name })
            })
            .collect()
    }

    /// 发送 SIGKILL 强制终止指定进程，返回是否成功。
    pub(super) fn terminate(pid: u32) -> bool {
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, name: &str) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            name: name.to_string(),
        }
    }

    #[test]
    fn kills_descendants_and_other_instances_but_not_self() {
        let procs = vec![
            proc(1, 0, "explorer.exe"),
            proc(10, 1, "termarx.exe"),     // 当前进程
            proc(11, 10, "pwsh.exe"),       // 当前进程的 shell
            proc(12, 11, "node.exe"),       // shell 的子进程
            proc(20, 99, "termarx.exe"),    // 常驻 host（父进程已退出）
            proc(21, 20, "OpenConsole.exe"),
            proc(22, 21, "cmd.exe"),
            proc(23, 22, "ping.exe"),
            proc(30, 1, "chrome.exe"),      // 无关进程
        ];
        let set = kill_set(&procs, 10, "TERMARX.exe");
        let killed: HashSet<u32> = set.iter().copied().collect();
        assert_eq!(killed, HashSet::from([11, 12, 20, 21, 22, 23]));
        assert!(!set.contains(&10));
        // 子孙必须排在祖先之前。
        let index = |pid| set.iter().position(|p| *p == pid).unwrap();
        assert!(index(23) < index(22) && index(22) < index(21) && index(21) < index(20));
    }

    #[test]
    fn tolerates_cycles_and_self_parent() {
        let procs = vec![
            proc(10, 10, "termarx"),
            proc(11, 12, "a"),
            proc(12, 11, "b"),
            proc(13, 10, "c"),
        ];
        assert_eq!(kill_set(&procs, 10, "termarx"), vec![13]);
    }
}

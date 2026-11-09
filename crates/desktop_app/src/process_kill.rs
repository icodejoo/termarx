//! “终止全部任务”的进程清理：杀掉终端里运行的任务（shell 的全部后代进程）。
//!
//! shell 本身和应用都不受影响，终端仍可继续输入命令。

use std::collections::{HashMap, HashSet, VecDeque};

/// 进程快照中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcEntry {
    /// 进程 ID。
    pub pid: u32,
    /// 父进程 ID。
    pub ppid: u32,
}

/// 计算 `roots` 的全部后代进程 ID（叶子优先，不含 `roots` 自身）。
///
/// - `procs`：当前系统的进程快照
/// - `roots`：各终端 shell 的进程 ID
///
/// 返回值按“子孙在前、祖先在后”排序，便于逐个终止。
pub(crate) fn descendants_of(procs: &[ProcEntry], roots: &[u32]) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs {
        // 自指或 ppid 为 0 的项没有意义，跳过以免成环。
        if proc.pid != proc.ppid && proc.ppid != 0 {
            children.entry(proc.ppid).or_default().push(proc.pid);
        }
    }

    let mut visited: HashSet<u32> = roots.iter().copied().collect();
    let mut order = Vec::new();
    let mut queue: VecDeque<u32> = roots.iter().copied().collect();
    while let Some(pid) = queue.pop_front() {
        for kid in children.get(&pid).into_iter().flatten() {
            if visited.insert(*kid) {
                order.push(*kid);
                queue.push_back(*kid);
            }
        }
    }
    // 广度优先的逆序即“子孙在前”。
    order.reverse();
    order
}

/// 终止给定 shell 进程的全部后代，返回成功终止的数量。
///
/// 使用示例：`process_kill::kill_descendants_of(&[shell_pid])` 结束该终端里正在跑的任务。
pub(crate) fn kill_descendants_of(roots: &[u32]) -> usize {
    if roots.is_empty() {
        return 0;
    }
    let procs = platform::snapshot();
    descendants_of(&procs, roots)
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
                procs.push(ProcEntry {
                    pid: entry.th32ProcessID,
                    ppid: entry.th32ParentProcessID,
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
            .args(["-axo", "pid=,ppid="])
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
                Some(ProcEntry { pid, ppid })
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

    fn proc(pid: u32, ppid: u32) -> ProcEntry {
        ProcEntry { pid, ppid }
    }

    #[test]
    fn kills_descendants_but_keeps_shell_and_unrelated() {
        let procs = vec![
            proc(1, 0),
            proc(10, 1),  // shell
            proc(11, 10), // 任务
            proc(12, 11), // 任务的子进程
            proc(20, 1),  // 另一个 shell
            proc(21, 20),
            proc(30, 1), // 无关进程
        ];
        assert_eq!(descendants_of(&procs, &[10]), vec![12, 11]);
        let both = descendants_of(&procs, &[10, 20]);
        assert_eq!(
            both.iter().copied().collect::<HashSet<_>>(),
            HashSet::from([11, 12, 21])
        );
        assert!(!both.contains(&10) && !both.contains(&20));
    }

    #[test]
    fn tolerates_cycles_self_parent_and_empty_roots() {
        let procs = vec![proc(10, 10), proc(11, 12), proc(12, 11), proc(13, 10)];
        assert_eq!(descendants_of(&procs, &[10]), vec![13]);
        assert!(descendants_of(&procs, &[]).is_empty());
    }
}

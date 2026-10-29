use super::*;
use sysinfo::{Pid, ProcessesToUpdate, System};

/// Ctrl+C 对应的控制字符，终端里的前台程序收到后会被中断。
const INTERRUPT_BYTE: &[u8] = b"\x03";

/// 从 `(pid, 父 pid)` 列表里收集 `root` 的所有后代，按"先父后子"排序。
///
/// - `processes`：系统进程快照，每项为 `(pid, parent_pid)`。
/// - `root`：起点进程，不包含在返回值里。
///
/// 返回值：后代进程 pid；强制结束时应倒序处理（先杀子孙）。
fn descendant_pids(processes: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for &(pid, parent_pid) in processes {
            if parent_pid == parent && pid != root && !found.contains(&pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found
}

impl TerminalView {
    /// 给指定窗格发 Ctrl+C，中断它的前台任务（窗格和 shell 保留）。
    ///
    /// 返回值：是否成功写入；窗格不存在时为 `false`。
    pub(in crate::terminal_view) fn interrupt_pane_task(&self, pane_id: &str) -> bool {
        self.send_input_to_pane(pane_id, INTERRUPT_BYTE)
    }

    /// 强制结束指定窗格里 shell 的全部子孙进程（shell 本身和窗格保留）。
    ///
    /// 枚举进程较慢，放到后台线程里做，不阻塞界面。
    /// 返回值：是否拿到了窗格 shell 的 pid；tmux 运行时拿不到，返回 `false`。
    pub(in crate::terminal_view) fn force_stop_pane_task(&self, pane_id: &str) -> bool {
        let Some(shell_pid) = self
            .pane_terminal_by_id(pane_id)
            .and_then(Terminal::child_pid)
        else {
            return false;
        };
        std::thread::spawn(move || {
            let mut system = System::new();
            system.refresh_processes(ProcessesToUpdate::All, true);
            let snapshot: Vec<(u32, u32)> = system
                .processes()
                .iter()
                .filter_map(|(pid, process)| {
                    process
                        .parent()
                        .map(|parent| (pid.as_u32(), parent.as_u32()))
                })
                .collect();
            for pid in descendant_pids(&snapshot, shell_pid).into_iter().rev() {
                if let Some(process) = system.process(Pid::from_u32(pid)) {
                    process.kill();
                }
            }
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendant_pids_collects_nested_children_only() {
        // 1 -> 2 -> 3 -> 4，另有无关的 9 -> 10
        let processes = [(2, 1), (3, 2), (4, 3), (10, 9), (1, 0)];
        assert_eq!(descendant_pids(&processes, 2), vec![3, 4]);
        assert_eq!(descendant_pids(&processes, 4), Vec::<u32>::new());
    }

    #[test]
    fn descendant_pids_survives_parent_cycles() {
        let processes = [(2, 1), (1, 2)];
        assert_eq!(descendant_pids(&processes, 1), vec![2]);
    }
}

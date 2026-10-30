//! Windows DLL 搜索路径加固。

/// 把本进程的 DLL 搜索范围收紧为"应用目录 + System32 + `AddDllDirectory` 加的目录"，不再读 PATH。
///
/// alacritty_terminal 会用 `LoadLibraryW("conpty.dll")` 优先加载新版 OpenConsole。不收紧的话，
/// PATH 里别的终端软件自带的 `conpty.dll` 会被悄悄捡到，ConPTY 行为随之改变
/// （例如把应用的鼠标模式转发给终端，却不在应用退出时关闭）。
///
/// 进程级设置、不可撤销，要在首次创建 PTY 之前调用，越早越好。想用自带的新版 ConPTY，
/// 把 `conpty.dll` 和 `OpenConsole.exe` 放到可执行文件同一目录即可。
///
/// # 返回值
/// 设置成功返回 `true`；失败，或非 Windows 平台（无操作），返回 `false`。
///
/// # 示例
/// ```no_run
/// // 放在 main 的第一行
/// termy_core::restrict_dll_search_path();
/// ```
pub fn restrict_dll_search_path() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
        };
        // SAFETY: 只设置进程级的 DLL 搜索策略，不涉及任何指针或句柄。
        unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::restrict_dll_search_path;

    #[test]
    fn is_a_noop_off_windows() {
        assert!(!restrict_dll_search_path());
    }
}

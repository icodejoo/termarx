//! 验证 `restrict_dll_search_path` 之后不再从 PATH 加载 DLL。
//!
//! 该设置是进程级、不可撤销的，所以这个文件只放一个测试，独占一个测试进程。
#![cfg(windows)]

use std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path};
use windows_sys::Win32::{Foundation::FreeLibrary, System::LibraryLoader::LoadLibraryW};

/// 按当前搜索规则加载一次 DLL；能加载返回 true，并立刻释放。
fn can_load(name: &str) -> bool {
    let wide: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` 以 NUL 结尾且在调用期间有效；成功拿到的句柄立刻释放。
    unsafe {
        let handle = LoadLibraryW(wide.as_ptr());
        if handle.is_null() {
            return false;
        }
        FreeLibrary(handle);
    }
    true
}

#[test]
fn restricted_search_ignores_path_directories() {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let name = format!("termy_dll_probe_{}.dll", std::process::id());
    let system_root = std::env::var_os("SystemRoot").expect("应有 SystemRoot");
    std::fs::copy(
        Path::new(&system_root).join("System32").join("version.dll"),
        dir.path().join(&name),
    )
    .expect("复制探针 DLL");

    let mut entries = vec![dir.path().to_path_buf()];
    entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    // SAFETY: 本文件只有这一个测试，修改环境变量时没有其他线程在读。
    unsafe { std::env::set_var("PATH", std::env::join_paths(entries).expect("拼接 PATH")) };

    assert!(can_load(&name), "收紧前应能从 PATH 加载到探针 DLL");

    assert!(termy_core::restrict_dll_search_path());

    assert!(!can_load(&name), "收紧后不应再从 PATH 加载");
    assert!(can_load("kernel32.dll"), "System32 里的 DLL 仍应可加载");
}

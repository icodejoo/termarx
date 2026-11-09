use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
pub use termy_core::release_core::ReleaseInfo;
pub use termy_core::release_core::{UpdateCheck, check_for_updates};

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available {
        version: String,
        asset_name: String,
        url: String,
        checksum_asset_name: Option<String>,
        checksum_url: Option<String>,
        extension: String,
    },
    Downloading {
        version: String,
        downloaded: u64,
        total: u64,
    },
    Downloaded {
        version: String,
        installer_path: PathBuf,
    },
    Installing {
        version: String,
    },
    InstallerLaunched {
        version: String,
    },
    Installed {
        version: String,
    },
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum InstallOutcome {
    Installed,
    InstallerLaunched,
}

#[cfg(target_os = "macos")]
pub fn cache_installer_path(version: &str, extension: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let cache_dir = PathBuf::from(home).join("Library/Caches/Termarx");
    let _ = std::fs::create_dir_all(&cache_dir);
    cache_dir.join(format!("update-{version}.{extension}"))
}

#[cfg(target_os = "windows")]
pub fn cache_installer_path(version: &str, extension: &str) -> PathBuf {
    let cache_dir = std::env::var("LOCALAPPDATA")
        .map_or_else(|_| std::env::temp_dir(), PathBuf::from)
        .join("Termarx")
        .join("Cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    cache_dir.join(format!("update-{version}.{extension}"))
}

#[cfg(target_os = "linux")]
pub fn cache_installer_path(version: &str, extension: &str) -> PathBuf {
    // Use XDG_CACHE_HOME or ~/.cache
    let cache_dir = std::env::var("XDG_CACHE_HOME")
        .map_or_else(
            |_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                PathBuf::from(home).join(".cache")
            },
            PathBuf::from,
        )
        .join("termarx");
    let _ = std::fs::create_dir_all(&cache_dir);
    cache_dir.join(format!("update-{version}.{extension}"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub fn cache_installer_path(version: &str, extension: &str) -> PathBuf {
    let cache_dir = std::env::temp_dir().join("termy-updates");
    let _ = std::fs::create_dir_all(&cache_dir);
    cache_dir.join(format!("update-{}.{}", version, extension))
}

pub fn download_installer(
    url: &str,
    dest: &Path,
    progress_tx: flume::Sender<(u64, u64)>,
) -> Result<PathBuf> {
    let response = ureq::get(url)
        .set("User-Agent", "Termy-Updater/1.0")
        .call()
        .context(t!("Failed to download installer"))?;

    let total: u64 = response
        .header("Content-Length")
        .and_then(|h| h.parse().ok())
        .unwrap_or(0);

    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(dest).context(t!("Failed to create installer file"))?;
    let mut downloaded: u64 = 0;
    let mut buf = [0u8; 65536]; // 64KiB chunks

    loop {
        let n = reader
            .read(&mut buf)
            .context(t!("Failed to read download stream"))?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n])?;
        downloaded += n as u64;
        let _ = progress_tx.try_send((downloaded, total));
    }

    Ok(dest.to_path_buf())
}

pub fn verify_installer_checksum(
    installer_path: &Path,
    asset_name: &str,
    checksum_asset_name: Option<&str>,
    checksum_url: Option<&str>,
) -> Result<()> {
    let Some(checksum_url) = checksum_url else {
        if checksum_required_for_current_platform() {
            anyhow::bail!(t!(
                "Release is missing a checksum asset for {asset_name}",
                asset_name = asset_name
            ));
        }

        log::warn!("Release has no checksum asset for {asset_name}; skipping verification");
        return Ok(());
    };

    let checksum_text = download_checksum_text(checksum_url)?;
    let allow_hash_only = checksum_asset_name.is_some_and(|checksum_asset_name| {
        checksum_asset_name.eq_ignore_ascii_case(&format!("{asset_name}.sha256"))
    });
    let expected = expected_sha256_for_asset(&checksum_text, asset_name, allow_hash_only)
        .with_context(|| {
            t!(
                "Checksum file did not contain an entry for {asset_name}",
                asset_name = asset_name
            )
        })?;
    let actual = file_sha256_hex(installer_path)?;

    if !actual.eq_ignore_ascii_case(&expected) {
        anyhow::bail!(t!(
            "Checksum mismatch for {asset_name}: expected {expected}, got {actual}",
            asset_name = asset_name,
            expected = expected,
            actual = actual
        ));
    }

    Ok(())
}

/// 当前平台是否强制要求校验和（Windows 与未签名的 macOS DMG 必须校验）。
fn checksum_required_for_current_platform() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}

fn download_checksum_text(url: &str) -> Result<String> {
    ureq::get(url)
        .set("User-Agent", "Termy-Updater/1.0")
        .call()
        .context(t!("Failed to download checksum file"))?
        .into_string()
        .context(t!("Failed to read checksum file"))
}

fn expected_sha256_for_asset(
    checksum_text: &str,
    asset_name: &str,
    allow_hash_only: bool,
) -> Option<String> {
    let mut only_hash = None;
    let mut hash_count = 0usize;

    for line in checksum_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let hashes = line
            .split_whitespace()
            .filter(|part| is_sha256_hex(part.trim()))
            .collect::<Vec<_>>();
        for hash in hashes {
            hash_count += 1;
            only_hash = Some(hash.to_ascii_lowercase());
            if checksum_line_matches_asset(line, asset_name) {
                return Some(hash.to_ascii_lowercase());
            }
        }
    }

    (allow_hash_only && hash_count == 1).then_some(only_hash?)
}

fn checksum_line_matches_asset(line: &str, asset_name: &str) -> bool {
    let normalized_asset = normalize_checksum_name(asset_name);
    line.split_whitespace()
        .skip(1)
        .map(normalize_checksum_name)
        .any(|part| part == normalized_asset)
}

fn normalize_checksum_name(name: &str) -> String {
    name.trim_matches(|ch| ch == '*' || ch == '"' || ch == '\'')
        .trim_start_matches("./")
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn file_sha256_hex(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).context(t!("Failed to open downloaded installer"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];

    loop {
        let n = file
            .read(&mut buf)
            .context(t!("Failed to read downloaded installer for checksum"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(target_os = "macos")]
pub fn do_install(dmg_path: &Path) -> Result<InstallOutcome> {
    use std::process::Command;

    // Mount the DMG
    let mount = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly"])
        .arg(dmg_path)
        .output()
        .context(t!("Failed to mount DMG"))?;

    if !mount.status.success() {
        anyhow::bail!(t!(
            "hdiutil attach failed: {error}",
            error = String::from_utf8_lossy(&mount.stderr)
        ));
    }

    let mount_stdout = String::from_utf8_lossy(&mount.stdout);
    let mount_point = mount_stdout
        .lines()
        .find_map(|line| {
            line.find("/Volumes/")
                .map(|start| PathBuf::from(line[start..].trim()))
        })
        .with_context(|| {
            t!(
                "Could not determine mounted volume from hdiutil output: {output}",
                output = mount_stdout.trim()
            )
        })?;

    let install_result: Result<()> = (|| {
        let mut app_path = None;
        for entry in std::fs::read_dir(&mount_point).context(t!("Failed to read mounted volume"))? {
            let entry = entry?;
            let path = entry.path();
            let is_app = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"));
            if !is_app {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("Termarx.app") {
                app_path = Some(path);
                break;
            }
            if app_path.is_none() {
                app_path = Some(path);
            }
        }

        let app_path = app_path.context(t!("No .app bundle found inside mounted DMG"))?;
        let target_app = PathBuf::from("/Applications").join(
            app_path
                .file_name()
                .context(t!("Mounted app bundle is missing file name"))?,
        );

        if target_app.exists() {
            let rm_result = Command::new("rm")
                .arg("-rf")
                .arg(&target_app)
                .output()
                .context(t!("Failed to remove old app bundle in /Applications"))?;
            if !rm_result.status.success() {
                anyhow::bail!(t!(
                    "failed removing existing app: {error}",
                    error = String::from_utf8_lossy(&rm_result.stderr)
                ));
            }
        }

        // Use ditto for macOS app bundles to preserve metadata and avoid nested .app copies.
        let copy_result = Command::new("ditto")
            .arg(&app_path)
            .arg(&target_app)
            .output()
            .context(t!("Failed to copy app bundle to /Applications"))?;

        if !copy_result.status.success() {
            anyhow::bail!(t!(
                "ditto failed: {error}",
                error = String::from_utf8_lossy(&copy_result.stderr)
            ));
        }

        Ok(())
    })();

    // Always try to detach, even if install failed.
    let _ = Command::new("hdiutil")
        .arg("detach")
        .arg(&mount_point)
        .arg("-quiet")
        .output();

    install_result.map(|()| InstallOutcome::Installed)
}

#[cfg(target_os = "windows")]
pub fn do_install(installer_path: &Path) -> Result<InstallOutcome> {
    let extension = installer_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match extension.as_str() {
        "msi" => {
            shell_execute_elevated(
                "msiexec.exe",
                &windows_msi_installer_parameters(installer_path),
            )
            .context(t!("Failed to launch MSI installer"))?;
        }
        "exe" => {
            stop_session_host();
            shell_execute_elevated(
                &installer_path.to_string_lossy(),
                &windows_exe_installer_parameters(),
            )
            .context(t!("Failed to launch EXE installer"))?;
        }
        _ => {
            anyhow::bail!(t!(
                "Unsupported installer format: {extension}",
                extension = extension
            ));
        }
    }

    Ok(InstallOutcome::InstallerLaunched)
}

/// 安装前优雅停止常驻的会话宿主（它会在应用退出后继续持有 shell 与 OpenConsole.exe，占用安装目录文件）。
///
/// 尽力而为：宿主没在运行或连接失败时直接忽略，交给安装器自己的关闭逻辑兜底。
#[cfg(target_os = "windows")]
fn stop_session_host() {
    let Some(root) = termy_core::config_core::config_path()
        .and_then(|path| path.parent().map(|dir| dir.join("multiplexer")))
    else {
        return;
    };
    if let Ok(client) = termy_core::multiplexer::SessionClient::connect(&root) {
        let _ = client.shutdown();
    }
}

#[cfg(target_os = "windows")]
fn shell_execute_elevated(file: &str, parameters: &str) -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let operation = wide_null("runas");
    let file = wide_null(file);
    let parameters = wide_null(parameters);

    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR::from_raw(operation.as_ptr()),
            PCWSTR::from_raw(file.as_ptr()),
            PCWSTR::from_raw(parameters.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code <= 32 {
        anyhow::bail!(t!("ShellExecuteW failed with code {code}", code = code));
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn wide_null(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(target_os = "windows")]
fn windows_msi_installer_parameters(installer_path: &Path) -> String {
    format!(
        "/i {} /passive /norestart",
        quote_windows_arg(&installer_path.to_string_lossy())
    )
}

#[cfg(target_os = "windows")]
fn windows_exe_installer_parameters() -> String {
    [
        "/SILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/CLOSEAPPLICATIONS",
    ]
    .join(" ")
}

#[cfg(target_os = "windows")]
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .bytes()
            .any(|byte| byte == b' ' || byte == b'\t' || byte == b'"')
    {
        return arg.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;

    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', (backslashes * 2) + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }

    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

// 注意：Linux 目前不支持自动更新，本函数暂不可达，故只拷启动脚本 `termarx`；
// 启用 Linux 自动更新前需补齐 termarx-bin / termarx-cli 的拷贝。
#[cfg(target_os = "linux")]
pub fn do_install(tarball_path: &Path) -> Result<InstallOutcome> {
    use std::process::Command;

    let home = std::env::var("HOME").context(t!("HOME environment variable not set"))?;
    let home_path = PathBuf::from(&home);

    let install_dir = if home_path.join(".local/bin").exists() {
        home_path.join(".local/bin")
    } else {
        let local_bin = home_path.join(".local/bin");
        std::fs::create_dir_all(&local_bin).context(t!("Failed to create ~/.local/bin"))?;
        local_bin
    };

    let temp_dir = std::env::temp_dir().join("termy-update-extract");
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).context(t!("Failed to create temp extraction directory"))?;

    let tar_result = Command::new("tar")
        .args([
            "-xzf",
            &tarball_path.to_string_lossy(),
            "-C",
            &temp_dir.to_string_lossy(),
        ])
        .output()
        .context(t!("Failed to extract tarball"))?;

    if !tar_result.status.success() {
        anyhow::bail!(t!(
            "tar extraction failed: {error}",
            error = String::from_utf8_lossy(&tar_result.stderr)
        ));
    }

    // Find the termarx binary in the extracted contents
    let binary_path = temp_dir.join("termarx/termarx");
    let alt_binary_path = temp_dir.join("termarx");

    let source_binary = if binary_path.exists() {
        binary_path
    } else if alt_binary_path.is_file() {
        alt_binary_path
    } else {
        // Search for the binary
        let mut found = None;
        for entry in std::fs::read_dir(&temp_dir).context(t!("Failed to read temp directory"))? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let potential = path.join("termarx");
                if potential.exists() {
                    found = Some(potential);
                    break;
                }
            }
        }
        found.context(t!("Could not find termarx binary in extracted tarball"))?
    };

    let target_binary = install_dir.join("termarx");
    std::fs::copy(&source_binary, &target_binary)
        .context(t!("Failed to copy binary to install directory"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&target_binary)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target_binary, perms)?;
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);

    Ok(InstallOutcome::Installed)
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub fn do_install(_installer_path: &Path) -> Result<InstallOutcome> {
    anyhow::bail!(t!(
        "Auto-install is only supported on macOS, Windows, and Linux"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// macOS 与 Windows 缺校验和时必须拒绝安装。
    #[test]
    fn checksum_required_on_macos_and_windows() {
        assert_eq!(
            checksum_required_for_current_platform(),
            cfg!(any(target_os = "windows", target_os = "macos"))
        );
    }

    /// 缺校验和 URL 时，需要强制校验的平台返回错误。
    #[test]
    fn missing_checksum_rejected_when_required() {
        let result = verify_installer_checksum(Path::new("nonexistent"), "a.dmg", None, None);
        assert_eq!(result.is_err(), checksum_required_for_current_platform());
    }

    #[test]
    fn checksum_parser_finds_matching_manifest_entry() {
        let checksums = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  Termarx-v1.2.3-macos-arm64.dmg
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *Termarx-v1.2.3-windows-x64-Setup.exe
";

        assert_eq!(
            expected_sha256_for_asset(checksums, "Termarx-v1.2.3-windows-x64-Setup.exe", false),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string())
        );
    }

    #[test]
    fn checksum_parser_accepts_single_hash_asset_file_when_allowed() {
        assert_eq!(
            expected_sha256_for_asset(
                "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
                "Termarx-v1.2.3-windows-x64-Setup.exe",
                true,
            ),
            Some("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_string())
        );
    }

    #[test]
    fn checksum_parser_rejects_hash_only_manifest_without_asset_name() {
        assert_eq!(
            expected_sha256_for_asset(
                "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
                "Termarx-v1.2.3-windows-x64-Setup.exe",
                false,
            ),
            None
        );
    }

    #[test]
    fn checksum_parser_rejects_ambiguous_hash_only_asset_file() {
        let checksums = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
";

        assert_eq!(
            expected_sha256_for_asset(checksums, "Termarx-v1.2.3-windows-x64-Setup.exe", true),
            None
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_exe_installer_uses_silent_handoff_flags() {
        assert_eq!(
            windows_exe_installer_parameters(),
            "/SILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_msi_installer_quotes_installer_path() {
        let params = windows_msi_installer_parameters(Path::new(
            "C:\\Users\\me\\Downloads\\Termarx Setup.msi",
        ));
        assert_eq!(
            params,
            "/i \"C:\\Users\\me\\Downloads\\Termarx Setup.msi\" /passive /norestart"
        );
    }
}

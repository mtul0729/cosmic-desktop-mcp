//! Discover the logged-in user's graphical session when the MCP client did
//! not forward its environment.
//!
//! Processes started by a long-lived user service, such as Magpie's gateway,
//! may predate the graphical session. Their children still belong to the same
//! user and can reach the session's socket, but `WAYLAND_DISPLAY` and
//! `XDG_SESSION_ID` are not in their environment.

use std::{ffi::OsStr, path::PathBuf, process::Command};

use anyhow::{Result, bail};

fn runtime_dir_from(xdg_runtime_dir: Option<&OsStr>) -> Result<PathBuf> {
    if let Some(dir) = xdg_runtime_dir.filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            bail!("XDG_RUNTIME_DIR is not absolute: {}", dir.display());
        }
        if !dir.is_dir() {
            bail!("XDG_RUNTIME_DIR is not a directory: {}", dir.display());
        }
        return Ok(dir);
    }

    let uid = unsafe { libc::geteuid() };
    let dir = PathBuf::from(format!("/run/user/{uid}"));
    if dir.is_dir() {
        Ok(dir)
    } else {
        bail!(
            "XDG_RUNTIME_DIR is not set and the user runtime directory {} does not exist",
            dir.display()
        )
    }
}

pub fn runtime_dir() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR");
    runtime_dir_from(dir.as_deref())
}

pub fn login_session_id() -> Option<String> {
    std::env::var("XDG_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(active_session_id)
}

fn active_session_id() -> Option<String> {
    let uid = unsafe { libc::geteuid() }.to_string();
    let output = Command::new("loginctl")
        .args(["show-user", &uid, "--property=Display", "--value"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_session_id(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

fn parse_session_id(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cosmic-desktop-mcp-session-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn runtime_directory_must_exist_and_be_absolute() {
        let temp = TempDir::new();
        assert_eq!(runtime_dir_from(Some(temp.0.as_os_str())).unwrap(), temp.0);
        assert!(runtime_dir_from(Some(OsStr::new("runtime"))).is_err());
        assert!(
            runtime_dir_from(Some(Path::new("/definitely/not/a/runtime/dir").as_os_str())).is_err()
        );
    }

    #[test]
    fn reads_the_display_session_from_loginctl() {
        assert_eq!(parse_session_id("4\n"), Some("4".to_string()));
        assert_eq!(parse_session_id("\n"), None);
    }
}

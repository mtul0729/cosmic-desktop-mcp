//! Discover the logged-in user's graphical session when the MCP client did
//! not forward its environment.
//!
//! Processes started by a long-lived user service, such as Magpie's gateway,
//! may predate the graphical session. Their children still belong to the same
//! user and can reach the session's socket, but `WAYLAND_DISPLAY` and
//! `XDG_SESSION_ID` are not in their environment.

use std::{path::PathBuf, process::Command};

use anyhow::{Result, bail};

pub fn runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            bail!("XDG_RUNTIME_DIR is not absolute: {}", dir.display());
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
    use super::*;

    #[test]
    fn reads_the_display_session_from_loginctl() {
        assert_eq!(parse_session_id("4\n"), Some("4".to_string()));
        assert_eq!(parse_session_id("\n"), None);
    }
}

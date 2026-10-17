//! Clipboard text, used as the transport for anything a keycode cannot express.
//!
//! `type_text` sends keycodes, so it only ever produces US-layout ASCII.
//! Chinese, emoji, or any other non-ASCII string has to travel as data: put it
//! on the clipboard, then send ctrl+v.
//!
//! This shells out to `wl-clipboard` instead of implementing
//! `ext_data_control_manager_v1` directly. A Wayland selection only exists
//! while its owning client is alive, so something has to stay resident either
//! way; wl-clipboard already handles that, the mime negotiation, and the
//! XWayland `STRING`/`UTF8_STRING` bridge that Electron and Chrome apps need.
//!
//! Consequence worth knowing: the payload lives exactly as long as this
//! process. When the MCP server exits, the clipboard entry goes with it.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// The `wl-copy --foreground` process currently serving the selection. Only
/// one may exist at a time; a new write replaces the old owner.
static OWNER: Mutex<Option<Child>> = Mutex::new(None);

pub fn write(text: &str) -> Result<()> {
    let mut owner = OWNER
        .lock()
        .map_err(|_| anyhow::anyhow!("clipboard lock poisoned"))?;

    if let Some(mut previous) = owner.take() {
        let _ = previous.kill();
        let _ = previous.wait();
    }

    let mut child = Command::new("wl-copy")
        .args(["--foreground", "--type", "text/plain;charset=utf-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn wl-copy (wl-clipboard must be on PATH)")?;

    {
        let stdin = child.stdin.as_mut().context("wl-copy stdin")?;
        stdin
            .write_all(text.as_bytes())
            .context("write clipboard payload")?;
    }
    // Closing stdin is what tells wl-copy the payload is complete.
    drop(child.stdin.take());

    // wl-copy exits immediately when it cannot take the selection, e.g. when
    // another client owns it and refuses to hand it over.
    std::thread::sleep(Duration::from_millis(150));
    if let Ok(Some(status)) = child.try_wait() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read as _;
            let _ = pipe.read_to_string(&mut stderr);
        }
        bail!("wl-copy exited early ({status}): {}", stderr.trim());
    }

    *owner = Some(child);
    Ok(())
}

pub fn read() -> Result<String> {
    let output = Command::new("wl-paste")
        .arg("--no-newline")
        .output()
        .context("spawn wl-paste (wl-clipboard must be on PATH)")?;
    if !output.status.success() {
        bail!(
            "wl-paste failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

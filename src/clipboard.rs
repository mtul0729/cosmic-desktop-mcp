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
//! The payload lives while the `wl-copy` owner is alive. Normal MCP server
//! shutdown terminates that owner; a hard kill may leave it behind.

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
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(mut previous) = owner.take() {
        stop(&mut previous);
    }

    let mut child = Command::new("wl-copy")
        .args(["--foreground", "--type", "text/plain;charset=utf-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn wl-copy (wl-clipboard must be on PATH)")?;

    let written = {
        let Some(stdin) = child.stdin.as_mut() else {
            stop(&mut child);
            bail!("wl-copy stdin was not available");
        };
        stdin
            .write_all(text.as_bytes())
            .context("write clipboard payload")
    };
    if let Err(err) = written {
        stop(&mut child);
        return Err(err);
    }
    // Closing stdin is what tells wl-copy the payload is complete.
    drop(child.stdin.take());

    // wl-copy exits immediately when it cannot take the selection, e.g. when
    // another client owns it and refuses to hand it over.
    std::thread::sleep(Duration::from_millis(150));
    match child.try_wait() {
        Ok(Some(status)) => {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read as _;
                let _ = pipe.read_to_string(&mut stderr);
            }
            bail!("wl-copy exited early ({status}): {}", stderr.trim());
        }
        Ok(None) => {}
        Err(err) => {
            stop(&mut child);
            return Err(err).context("check wl-copy after starting it");
        }
    }

    *owner = Some(child);
    Ok(())
}

/// Stops the process that currently owns the selection. Called on normal MCP
/// server shutdown; `Child` itself does not kill its process when dropped.
pub fn shutdown() {
    let mut owner = OWNER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(mut child) = owner.take() {
        stop(&mut child);
    }
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

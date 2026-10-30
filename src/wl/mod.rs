use std::{
    fs,
    os::unix::{ffi::OsStrExt, fs::FileTypeExt, net::UnixStream},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use wayland_client::Connection;

use crate::session;

pub mod capture;
pub mod toplevel;
pub mod workspace;

/// Connect to the compositor, discovering the session socket when a cleared
/// environment left out `WAYLAND_DISPLAY`.
pub fn connect() -> Result<Connection> {
    if std::env::var_os("WAYLAND_SOCKET").is_some() {
        return Connection::connect_to_env().context("connect to the inherited WAYLAND_SOCKET");
    }

    let socket = socket_path()?;
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("connect to Wayland compositor at {}", socket.display()))?;
    Connection::from_socket(stream).context("initialize the Wayland client")
}

/// The Unix socket used for this user's Wayland session.
///
/// An explicit `WAYLAND_DISPLAY` wins. Otherwise a single `wayland-*` socket
/// in the user runtime directory is accepted; multiple sockets need an
/// explicit choice rather than guessing which compositor belongs to the user.
pub fn socket_path() -> Result<PathBuf> {
    if let Some(display) = std::env::var_os("WAYLAND_DISPLAY").filter(|value| !value.is_empty()) {
        let display = PathBuf::from(display);
        if display.is_absolute() {
            return Ok(display);
        }
        return Ok(session::runtime_dir()?.join(display));
    }

    discover_socket(&session::runtime_dir()?)
}

fn discover_socket(runtime_dir: &Path) -> Result<PathBuf> {
    let mut sockets = Vec::new();
    for entry in fs::read_dir(runtime_dir)
        .with_context(|| format!("read the user runtime directory {}", runtime_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_name().as_bytes().starts_with(b"wayland-") {
            continue;
        }
        if entry.file_type()?.is_socket() {
            sockets.push(entry.path());
        }
    }
    sockets.sort();

    match sockets.as_slice() {
        [socket] => Ok(socket.clone()),
        [] => bail!(
            "WAYLAND_DISPLAY is not set and no Wayland socket was found in {}; set WAYLAND_DISPLAY \
             to the session socket",
            runtime_dir.display()
        ),
        _ => bail!(
            "WAYLAND_DISPLAY is not set and multiple Wayland sockets were found in {}: {}; set \
             WAYLAND_DISPLAY to the session socket",
            runtime_dir.display(),
            sockets
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::net::UnixListener,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cosmic-desktop-mcp-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn finds_the_only_wayland_socket_and_ignores_lock_files() {
        let dir = TempDir::new();
        let socket = dir.0.join("wayland-1");
        let _listener = UnixListener::bind(&socket).unwrap();
        fs::write(dir.0.join("wayland-1.lock"), []).unwrap();
        assert_eq!(discover_socket(&dir.0).unwrap(), socket);
    }

    #[test]
    fn refuses_to_guess_between_sockets() {
        let dir = TempDir::new();
        let _one = UnixListener::bind(dir.0.join("wayland-0")).unwrap();
        let _two = UnixListener::bind(dir.0.join("wayland-1")).unwrap();
        assert!(
            discover_socket(&dir.0)
                .unwrap_err()
                .to_string()
                .contains("multiple")
        );
    }
}

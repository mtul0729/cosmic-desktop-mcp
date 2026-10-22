use anyhow::{Context, Result, bail};
use wayland_client::Connection;

pub mod capture;
pub mod toplevel;
pub mod workspace;

/// Connect using the session variables this MCP server needs.
///
/// Some stdio MCP clients clear the environment before spawning a server.
/// Letting wayland-client fail on its own produces "Could not find wayland
/// compositor", which points at the compositor instead of the launch contract.
pub fn connect() -> Result<Connection> {
    let missing: Vec<&str> = ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_none_or(|value| value.is_empty()))
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing Wayland session environment: {}. Forward these variables from the MCP \
             client; for Codex add env_vars = [\"WAYLAND_DISPLAY\", \"XDG_RUNTIME_DIR\", \
             \"XDG_SESSION_ID\"] to the server configuration and restart the session",
            missing.join(", ")
        );
    }

    Connection::connect_to_env().context("connect to Wayland compositor")
}

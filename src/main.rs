//! MCP server for driving a COSMIC/Wayland desktop.
//!
//! Three layers, each using the protocol COSMIC actually implements:
//!   - `wl::capture`     ext_image_copy_capture_v1 + wl_shm
//!   - `wl::toplevel`    ext_foreign_toplevel_list_v1
//!   - `uinput`          /dev/uinput (COSMIC has no virtual-pointer protocol)

mod mcp;
mod uinput;
mod wl;

use std::process::ExitCode;

use anyhow::{Result, bail};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cosmic-desktop-mcp: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => mcp::serve(),
        Some("--doctor") => mcp::doctor(),
        Some("--version" | "-V") => {
            println!("cosmic-desktop-mcp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h") => {
            println!(
                "cosmic-desktop-mcp {}\n\
                 \n\
                 Usage:\n\
                 \x20 cosmic-desktop-mcp            run the MCP server on stdio\n\
                 \x20 cosmic-desktop-mcp --doctor   check wayland, toplevels, capture and uinput\n\
                 \x20 cosmic-desktop-mcp --version  print version\n",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some(other) => bail!("unknown argument {other:?} (try --help)"),
    }
}

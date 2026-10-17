//! MCP server for driving a COSMIC/Wayland desktop.
//!
//! Four layers, each using the protocol COSMIC actually implements:
//!   - `wl::capture`     ext_image_copy_capture_v1 + wl_shm
//!   - `wl::toplevel`    ext_foreign_toplevel_list_v1
//!   - `input`           EIS/libei through the RemoteDesktop portal
//!   - `keymap`          linux key/button codes (US layout)

mod clipboard;
mod eis;
mod input;
mod keymap;
mod mcp;
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
        Some("--eis-probe") => {
            let session = eis::session()?;
            print!("{}", session.describe());
            Ok(())
        }
        Some("--eis-click") => {
            let mut rest = std::env::args().skip(2);
            let x = rest.next().and_then(|v| v.parse::<f64>().ok());
            let y = rest.next().and_then(|v| v.parse::<f64>().ok());
            let (Some(x), Some(y)) = (x, y) else {
                bail!("--eis-click needs two logical coordinates, e.g. --eis-click 1024 576");
            };
            let session = eis::session()?;
            session.move_to(x, y)?;
            std::thread::sleep(std::time::Duration::from_millis(250));
            session.button(eis::BTN_LEFT, true)?;
            std::thread::sleep(std::time::Duration::from_millis(90));
            session.button(eis::BTN_LEFT, false)?;
            println!("clicked at ({x}, {y}) in logical coordinates");
            Ok(())
        }
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
                 \x20 cosmic-desktop-mcp --doctor   check wayland, toplevels, capture and the input backend\n\
                 \x20 cosmic-desktop-mcp --eis-probe  open a RemoteDesktop portal session and dump EIS devices\n\
                 \x20 cosmic-desktop-mcp --eis-click X Y  left-click a logical desktop coordinate over EIS\n\
                 \n\
                 \x20 cosmic-desktop-mcp --version  print version\n",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some(other) => bail!("unknown argument {other:?} (try --help)"),
    }
}

//! Optional input backend: libei, obtained through the XDG RemoteDesktop portal.
//!
//! Why this exists alongside `uinput`:
//!
//! - No `/dev/uinput`, no udev rule, no extra input device in the session.
//!   COSMIC sees a remote-desktop session, the same thing a VNC or RDP client
//!   would open, and it can be revoked from the portal.
//! - Coordinates are logical pixels, straight from the device region. The
//!   HiDPI question that `uinput` has (absolute range built from physical
//!   pixels) does not come up.
//!
//! The cost is a portal dialog on the first call of a session, so this stays
//! opt-in (`--eis-probe`) until it has proven itself.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use reis::ei;
use reis::event::{Connection, DeviceCapability, EiEvent};

/// Linux input-event codes, same values uinput would use.
const BTN_LEFT: u32 = 0x110;

/// Connects to the portal, negotiates a session and dumps what the compositor
/// grants. With `click`, also places one left click at the given logical
/// desktop coordinate — that is the part that proves the coordinate space.
pub fn probe(click: Option<(f32, f32)>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start async runtime")?;

    let context = runtime
        .block_on(open_session())
        .context("open an EIS session")?;

    let (connection, events) = context
        .handshake_blocking("cosmic-desktop-mcp", ei::handshake::ContextType::Sender)
        .context("EIS handshake")?;
    println!(
        "eis: handshake done (ei_connection v{})",
        connection.connection().version()
    );

    // The iterator blocks in poll(). `EiEvent` is not `Send` (it carries the
    // connection's callback map), so it cannot be moved to a worker thread —
    // the probe just waits and lets Ctrl-C end it.
    println!("eis: waiting for devices (Ctrl-C to stop)");

    let mut state = State::default();
    for event in events {
        let event = event.context("read EIS event")?;
        handle(event, &connection, click, &mut state)?;
        // reis queues outgoing requests in a buffer; without this the seat
        // bind (and the click) never leave the process. The upstream examples
        // flush at the end of every read cycle for the same reason.
        connection.flush().context("flush EIS requests")?;
        if state.clicked {
            // One click is the whole experiment; do not keep the session open.
            break;
        }
    }

    if state.devices == 0 {
        println!("eis: no device appeared — portal denied, or the dialog was dismissed");
    }
    if click.is_some() && state.clicked {
        // Stay connected briefly so the caller can screenshot while the
        // session is still alive; teardown right after the click makes it
        // impossible to tell a failed click from a popup closed by teardown.
        std::thread::sleep(Duration::from_secs(3));
    }
    Ok(())
}

#[derive(Default)]
struct State {
    devices: usize,
    clicked: bool,
}

fn handle(
    event: EiEvent,
    connection: &Connection,
    click: Option<(f32, f32)>,
    state: &mut State,
) -> Result<()> {
    match event {
        EiEvent::SeatAdded(added) => {
            println!("eis: seat {:?}", added.seat.name());
            // Ask for everything a click needs; the compositor answers with the
            // subset it is willing to emulate.
            added.seat.bind_capabilities(
                DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Keyboard,
            );
        }
        EiEvent::DeviceAdded(added) => {
            state.devices += 1;
            report_device(&added.device);
        }
        EiEvent::DeviceResumed(resumed) => {
            let Some((x, y)) = click else {
                return Ok(());
            };
            if state.clicked || resumed.device.interface::<ei::PointerAbsolute>().is_none() {
                return Ok(());
            }
            for region in resumed.device.regions() {
                let inside = x >= region.x as f32
                    && x < (region.x + region.width) as f32
                    && y >= region.y as f32
                    && y < (region.y + region.height) as f32;
                if inside {
                    println!(
                        "eis: ({x}, {y}) is inside {:?} at {}.{} scale {}",
                        region.mapping_id, region.x, region.y, region.scale
                    );
                }
            }
            click_at(&resumed.device, resumed.serial, x, y)?;
            connection.flush().context("flush EIS requests")?;
            state.clicked = true;
            println!("eis: click sent at ({x}, {y}) in logical coordinates");
        }
        _ => {}
    }
    Ok(())
}

fn report_device(device: &reis::event::Device) {
    println!(
        "eis: device type={:?} name={:?} dimensions={:?}",
        device.device_type(),
        device.name(),
        device.dimensions()
    );
    for region in device.regions() {
        println!(
            "eis:   region x={} y={} {}x{} scale={} mapping_id={:?}",
            region.x, region.y, region.width, region.height, region.scale, region.mapping_id
        );
    }
    println!(
        "eis:   interfaces pointer_absolute={} button={} keyboard={}",
        device.interface::<ei::PointerAbsolute>().is_some(),
        device.interface::<ei::Button>().is_some(),
        device.interface::<ei::Keyboard>().is_some(),
    );
}

fn click_at(device: &reis::event::Device, serial: u32, x: f32, y: f32) -> Result<()> {
    let raw = device.device();
    raw.start_emulating(serial, 1);

    if let Some(pointer) = device.interface::<ei::PointerAbsolute>() {
        pointer.motion_absolute(x, y);
    }
    if let Some(button) = device.interface::<ei::Button>() {
        button.button(BTN_LEFT, ei::button::ButtonState::Press);
        button.button(BTN_LEFT, ei::button::ButtonState::Released);
    } else {
        println!("eis: no ei_button interface — motion only, no click");
    }

    raw.frame(serial, monotonic_us());
    raw.stop_emulating(serial);
    Ok(())
}

fn monotonic_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safety: CLOCK_MONOTONIC always fills the struct on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

async fn open_session() -> Result<ei::Context> {
    let portal = RemoteDesktop::new()
        .await
        .context("connect to the RemoteDesktop portal")?;
    println!("portal: RemoteDesktop v{}", portal.version());
    println!(
        "portal: device types {:?}",
        portal
            .available_device_types()
            .await
            .context("query available device types")?
    );

    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .context("create portal session")?;

    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Pointer | DeviceType::Keyboard)
        // The token only survives if the session may persist; `DoNot` throws
        // it away and every probe would pop the dialog again.
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(load_token().as_deref());
    portal
        .select_devices(&session, options)
        .await
        .context("select devices")?
        .response()
        .context("select devices was rejected")?;

    println!("portal: waiting for the confirmation dialog…");
    let granted = portal
        .start(&session, None, StartOptions::default())
        .await
        .context("start portal session")?
        .response()
        .context("portal did not grant the session")?;
    println!("portal: granted {:?}", granted.devices());
    if let Some(token) = granted.restore_token() {
        store_token(token);
    }

    let fd = portal
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .context("connect to EIS")?;
    ei::Context::new(UnixStream::from(fd)).context("wrap the EIS socket")
}

/// `$XDG_STATE_HOME/cosmic-desktop-mcp/restore-token`, falling back to
/// `~/.local/state/...`. The token is per-user and not a secret in the
/// password sense, but it does grant remote input — keep it out of the repo.
fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("cosmic-desktop-mcp/restore-token"))
}

fn load_token() -> Option<String> {
    let path = token_path()?;
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

fn store_token(token: &str) {
    let Some(path) = token_path() else { return };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    if std::fs::write(&path, token).is_ok() {
        println!("portal: restore token saved to {}", path.display());
    }
}

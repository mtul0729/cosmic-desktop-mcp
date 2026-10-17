//! Input backend: libei, obtained through the XDG RemoteDesktop portal.
//!
//! Why this exists alongside `uinput`:
//!
//! - No `/dev/uinput`, no udev rule, no extra input device in the session.
//!   COSMIC sees a remote-desktop session, the same thing a VNC or RDP client
//!   would open, and it can be revoked from the portal.
//! - Coordinates are logical pixels, straight from the device region, so
//!   multi-monitor and fractional scaling work without any guesswork.
//!
//! The session is opened once and kept for the life of the process: the portal
//! only asks for authorization the first time (the restore token is persisted),
//! and every later call is a few protocol messages.
//!
//! Threading. `EiEvent` is not `Send` — it carries the converter's callback
//! map — so the event loop owns the connection and runs on its own thread.
//! What it publishes is a shared `State` of live devices; the caller sends
//! input from its own thread through those device handles and flushes through
//! the shared `Connection`. Buffering commands behind the blocking event read
//! is not an option: that read has no timeout of its own.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use reis::ei;
use reis::event::{Connection, DeviceCapability, EiEvent};

/// Linux input-event codes; the same numbering `uinput` uses.
pub const BTN_LEFT: u32 = 0x110;

/// Opening the session can need a person to click "allow", so the first call
/// waits far longer than a normal request would.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the compositor to resume a device after the seat bind.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(10);

/// One monitor's place in the logical desktop, as reported by the compositor.
#[derive(Clone, Debug)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    pub mapping_id: Option<String>,
}

impl Region {
    /// Physical size of the region, i.e. what a screenshot of it measures.
    pub fn physical_size(&self) -> (i32, i32) {
        (
            (self.width as f64 * self.scale).round() as i32,
            (self.height as f64 * self.scale).round() as i32,
        )
    }
}

#[derive(Clone)]
pub struct Session {
    connection: Connection,
    state: Arc<Mutex<State>>,
    alive: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    devices: Vec<Live>,
    regions: Vec<Region>,
}

struct Live {
    device: reis::event::Device,
    name: Option<String>,
    serial: u32,
    absolute: bool,
    button: bool,
    keyboard: bool,
    pointer: bool,
    scroll: bool,
}

static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

/// Returns the process-wide EIS session, opening it on first use.
pub fn session() -> Result<Session> {
    let slot = SESSION.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = guard.as_ref() {
        if existing.is_alive() {
            return Ok(existing.clone());
        }
    }
    let fresh = Session::open()?;
    *guard = Some(fresh.clone());
    Ok(fresh)
}

impl Session {
    fn open() -> Result<Self> {
        let alive = Arc::new(AtomicBool::new(true));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_alive = Arc::clone(&alive);

        std::thread::Builder::new()
            .name("eis-session".to_string())
            .spawn(move || run(ready_tx, thread_alive))
            .context("spawn the EIS session thread")?;

        match ready_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(session)) => Ok(session),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Timeout) => bail!(
                "the EIS session did not come up within {}s — is the portal authorization dialog \
                 waiting for an answer?",
                STARTUP_TIMEOUT.as_secs()
            ),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("the EIS session thread died while starting")
            }
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn regions(&self) -> Vec<Region> {
        self.state().regions.clone()
    }

    /// Moves the pointer to a logical desktop coordinate.
    pub fn move_to(&self, x: f64, y: f64) -> Result<()> {
        let state = self.state();
        let live = state
            .pick_absolute()
            .context("the compositor granted no absolute pointer")?;
        with_frame(live, || {
            if let Some(pointer) = live.device.interface::<ei::PointerAbsolute>() {
                pointer.motion_absolute(x as f32, y as f32);
            }
        });
        self.flush()
    }

    pub fn button(&self, code: u32, pressed: bool) -> Result<()> {
        let state = self.state();
        let live = state
            .pick_button()
            .context("the compositor granted no button interface")?;
        let button = live
            .device
            .interface::<ei::Button>()
            .context("the compositor granted no button interface")?;
        with_frame(live, || {
            button.button(
                code,
                if pressed {
                    ei::button::ButtonState::Press
                } else {
                    ei::button::ButtonState::Released
                },
            );
        });
        self.flush()
    }

    /// Scrolls by whole wheel clicks, the unit the tools speak in. `ei_scroll`
    /// measures in multiples of 120, and its y axis points down.
    pub fn scroll_clicks(&self, dx: i32, dy: i32) -> Result<()> {
        let state = self.state();
        let live = state.pick_scroll().context(
            "the compositor granted no scroll interface, so the wheel is unavailable over EIS",
        )?;
        let scroll = live.device.interface::<ei::Scroll>().context(
            "the compositor granted no scroll interface, so the wheel is unavailable over EIS",
        )?;
        with_frame(live, || scroll.scroll_discrete(dx * 120, dy * 120));
        self.flush()
    }

    pub fn key(&self, code: u32, pressed: bool) -> Result<()> {
        let state = self.state();
        let live = state
            .pick_keyboard()
            .context("the compositor granted no keyboard")?;
        let keyboard = live
            .device
            .interface::<ei::Keyboard>()
            .context("the compositor granted no keyboard")?;
        with_frame(live, || {
            keyboard.key(
                code,
                if pressed {
                    ei::keyboard::KeyState::Press
                } else {
                    ei::keyboard::KeyState::Released
                },
            );
        });
        self.flush()
    }

    pub fn describe(&self) -> String {
        let state = self.state();
        let mut out = String::new();
        if state.devices.is_empty() {
            out.push_str("devices: none\n");
        }
        for live in &state.devices {
            out.push_str(&format!(
                "device {:?} serial={} absolute={} button={} keyboard={} pointer={} scroll={}\n",
                live.name.as_deref().unwrap_or("<unnamed>"),
                live.serial,
                live.absolute,
                live.button,
                live.keyboard,
                live.pointer,
                live.scroll
            ));
        }
        for region in &state.regions {
            let (px, py) = region.physical_size();
            out.push_str(&format!(
                "region {:?} logical x={} y={} {}x{} scale={} -> physical {px}x{py}\n",
                region.mapping_id, region.x, region.y, region.width, region.height, region.scale
            ));
        }
        out
    }

    fn flush(&self) -> Result<()> {
        self.connection.flush().context("flush EIS requests")
    }
}

impl State {
    fn pick_absolute(&self) -> Option<&Live> {
        self.devices.iter().find(|live| live.absolute)
    }

    fn pick_button(&self) -> Option<&Live> {
        self.devices.iter().find(|live| live.button)
    }

    fn pick_keyboard(&self) -> Option<&Live> {
        self.devices.iter().find(|live| live.keyboard)
    }

    fn pick_scroll(&self) -> Option<&Live> {
        self.devices.iter().find(|live| live.scroll)
    }
}

/// Wraps one batch of events in the emulating / frame / stop-emulating
/// sequence EIS requires around every logical frame.
fn with_frame(live: &Live, build: impl FnOnce()) {
    let raw = live.device.device();
    raw.start_emulating(live.serial, 1);
    build();
    raw.frame(live.serial, monotonic_us());
    raw.stop_emulating(live.serial);
}

/// Owns the connection for the life of the process. Everything that is not
/// `Send` — the converter, the event iterator — stays on this thread.
fn run(ready: Sender<Result<Session>>, alive: Arc<AtomicBool>) {
    let mut ready = Some(ready);

    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            fail(ready, alive, anyhow::anyhow!("start async runtime: {err}"));
            return;
        }
    };

    let context = match runtime.block_on(open_session()) {
        Ok(context) => context,
        Err(err) => {
            fail(ready, alive, err.context("open an EIS session"));
            return;
        }
    };

    let (connection, events) =
        match context.handshake_blocking("cosmic-desktop-mcp", ei::handshake::ContextType::Sender) {
            Ok(pair) => pair,
            Err(err) => {
                fail(ready, alive, anyhow::anyhow!("EIS handshake: {err}"));
                return;
            }
        };

    let state = Arc::new(Mutex::new(State::default()));
    let started = Instant::now();

    for event in events {
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                eprintln!("eis: read error: {err}");
                break;
            }
        };

        {
            let mut guard = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if !handle(&mut guard, event) {
                alive.store(false, Ordering::Relaxed);
                return;
            }
            // Publish as soon as a usable device exists: the caller would
            // otherwise race the first DeviceResumed and see "no pointer".
            if ready.is_some() && !guard.devices.is_empty() {
                let session = Session {
                    connection: connection.clone(),
                    state: Arc::clone(&state),
                    alive: Arc::clone(&alive),
                };
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok(session));
                }
            }
        }

        // reis buffers outgoing requests; without this the seat bind never
        // leaves the process and no device ever appears.
        if let Err(err) = connection.flush() {
            eprintln!("eis: flush failed: {err}");
            break;
        }

        if ready.is_some() && started.elapsed() > DEVICE_TIMEOUT {
            if let Some(tx) = ready.take() {
                let _ = tx.send(Err(anyhow::anyhow!(
                    "the portal granted no input devices — authorization was probably denied"
                )));
            }
            alive.store(false, Ordering::Relaxed);
            return;
        }
    }

    if let Some(tx) = ready.take() {
        let _ = tx.send(Err(anyhow::anyhow!("the EIS session closed before any device appeared")));
    }
    alive.store(false, Ordering::Relaxed);
}

fn fail(mut ready: Option<Sender<Result<Session>>>, alive: Arc<AtomicBool>, err: anyhow::Error) {
    if let Some(tx) = ready.take() {
        let _ = tx.send(Err(err));
    }
    alive.store(false, Ordering::Relaxed);
}

/// Returns `false` when the connection is gone.
fn handle(state: &mut State, event: EiEvent) -> bool {
    match event {
        EiEvent::Disconnected(_) => return false,
        EiEvent::SeatAdded(added) => {
            // Ask for everything the tools need; the compositor answers with
            // the subset it is willing to emulate.
            added.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll
                    | DeviceCapability::Keyboard,
            );
        }
        EiEvent::DeviceResumed(resumed) => {
            let device = &resumed.device;
            let live = Live {
                name: device.name().map(str::to_owned),
                serial: resumed.serial,
                absolute: device.interface::<ei::PointerAbsolute>().is_some(),
                button: device.interface::<ei::Button>().is_some(),
                keyboard: device.interface::<ei::Keyboard>().is_some(),
                pointer: device.interface::<ei::Pointer>().is_some(),
                scroll: device.interface::<ei::Scroll>().is_some(),
                device: device.clone(),
            };
            if live.absolute {
                state.regions = device
                    .regions()
                    .iter()
                    .map(|region| Region {
                        x: region.x as f64,
                        y: region.y as f64,
                        width: region.width as i32,
                        height: region.height as i32,
                        scale: region.scale as f64,
                        mapping_id: region.mapping_id.clone(),
                    })
                    .collect();
            }
            state.devices.retain(|other| other.serial != live.serial);
            state.devices.push(live);
        }
        EiEvent::DevicePaused(paused) => {
            state.devices.retain(|live| live.serial != paused.serial);
        }
        EiEvent::DeviceRemoved(removed) => {
            let gone = removed.device.name().map(str::to_owned);
            state.devices.retain(|live| live.name != gone);
        }
        _ => {}
    }
    true
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

    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .context("create portal session")?;

    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Pointer | DeviceType::Keyboard)
        // The token only survives if the session may persist; `DoNot` throws
        // it away and every start would pop the dialog again.
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(load_token().as_deref());
    portal
        .select_devices(&session, options)
        .await
        .context("select devices")?
        .response()
        .context("select devices was rejected")?;

    let granted = portal
        .start(&session, None, StartOptions::default())
        .await
        .context("start portal session")?
        .response()
        .context("the portal did not grant the session")?;
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
    let _ = std::fs::write(&path, token);
}

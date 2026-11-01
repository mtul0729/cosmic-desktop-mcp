//! Input backend: libei, obtained through the XDG RemoteDesktop portal.
//!
//! Why it is done this way:
//!
//! - COSMIC has no `zwp_virtual_pointer_manager_v1`, so pointer buttons and the
//!   wheel cannot be injected with a Wayland protocol. EIS is the compositor's
//!   own input-injection path, and it needs no `/dev/uinput`, no udev rule and
//!   no extra input device in the session. COSMIC sees a remote-desktop
//!   session — the same thing a VNC or RDP client would open — and it can be
//!   revoked from the portal.
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

use std::ffi::OsStr;
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use reis::ei;
use reis::event::{Connection, DeviceCapability, EiEvent};

/// Linux input-event codes; the same numbering the kernel uses.
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
    token_warning: Option<String>,
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
    if let Some(existing) = guard.as_ref()
        && existing.is_alive()
    {
        return Ok(existing.clone());
    }
    let fresh = Session::open()?;
    *guard = Some(fresh.clone());
    Ok(fresh)
}

/// Returns the process-wide EIS session **if one is already open**.
///
/// Unlike [`session`] this never starts a session, so it can be used for status
/// reporting: opening one may put a portal authorization dialog on screen, and
/// a status query must not do that.
pub fn peek() -> Option<Session> {
    let slot = SESSION.get()?;
    let guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref().filter(|session| session.is_alive()).cloned()
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
        let mut out = String::new();
        for line in self.summary_lines() {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Same facts as [`Session::describe`], one line per device and per region,
    /// so a JSON caller can embed them without parsing prose.
    pub fn summary_lines(&self) -> Vec<String> {
        let state = self.state();
        let mut out = Vec::new();
        if state.devices.is_empty() {
            out.push("devices: none".to_string());
        }
        for live in &state.devices {
            out.push(format!(
                "device {:?} serial={} absolute={} button={} keyboard={} pointer={} scroll={}",
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
            out.push(format!(
                "region {:?} logical x={} y={} {}x{} scale={} -> physical {px}x{py}",
                region.mapping_id, region.x, region.y, region.width, region.height, region.scale
            ));
        }
        if let Some(warning) = &self.token_warning {
            out.push(format!("warning: {warning}"));
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

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            fail(ready, alive, anyhow::anyhow!("start async runtime: {err}"));
            return;
        }
    };

    let (context, token_warning) = match runtime.block_on(open_session()) {
        Ok(opened) => opened,
        Err(err) => {
            fail(ready, alive, err.context("open an EIS session"));
            return;
        }
    };
    if let Some(warning) = &token_warning {
        eprintln!("eis: {warning}");
    }

    let (connection, events) = match context
        .handshake_blocking("cosmic-desktop-mcp", ei::handshake::ContextType::Sender)
    {
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
            let mut guard = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
                    token_warning: token_warning.clone(),
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
        let _ = tx.send(Err(anyhow::anyhow!(
            "the EIS session closed before any device appeared"
        )));
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

async fn open_session() -> Result<(ei::Context, Option<String>)> {
    let portal = RemoteDesktop::new()
        .await
        .context("connect to the RemoteDesktop portal")?;

    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .context("create portal session")?;

    let (restore_token, mut warning) = load_token();
    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Pointer | DeviceType::Keyboard)
        // The token only survives if the session may persist; `DoNot` throws
        // it away and every start would pop the dialog again.
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
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
    if let Some(token) = granted.restore_token()
        && let Err(err) = store_token(token)
    {
        append_warning(
            &mut warning,
            format!("could not persist restore token: {err:#}"),
        );
    }

    let fd = portal
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .context("connect to EIS")?;
    Ok((
        ei::Context::new(UnixStream::from(fd)).context("wrap the EIS socket")?,
        warning,
    ))
}

fn state_base_from(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    if let Some(dir) = xdg_state_home.filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            bail!("XDG_STATE_HOME is not absolute: {}", dir.display());
        }
        return Ok(dir);
    }

    let home = home
        .filter(|value| !value.is_empty())
        .context("neither XDG_STATE_HOME nor HOME is set")?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        bail!("HOME is not absolute: {}", home.display());
    }
    Ok(home.join(".local/state"))
}

/// `$XDG_STATE_HOME/cosmic-desktop-mcp/restore-token`, falling back to
/// `~/.local/state/...`. The token grants remote input, so the file and its
/// directory are kept private.
fn token_path() -> Result<PathBuf> {
    let xdg_state_home = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    Ok(state_base_from(xdg_state_home.as_deref(), home.as_deref())?
        .join("cosmic-desktop-mcp/restore-token"))
}

fn load_token() -> (Option<String>, Option<String>) {
    let path = match token_path() {
        Ok(path) => path,
        Err(err) => {
            return (
                None,
                Some(format!("could not locate the restore token: {err:#}")),
            );
        }
    };
    load_token_at(&path)
}

fn load_token_at(path: &Path) -> (Option<String>, Option<String>) {
    let mut warning = None;
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                match std::fs::set_permissions(path, Permissions::from_mode(0o600)) {
                    Ok(()) => append_warning(
                        &mut warning,
                        format!(
                            "restore token at {} had permissions {mode:03o}; tightened to 600",
                            path.display()
                        ),
                    ),
                    Err(err) => append_warning(
                        &mut warning,
                        format!(
                            "restore token at {} has permissions {mode:03o} and could not be \
                             tightened to 600: {err}",
                            path.display()
                        ),
                    ),
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            append_warning(
                &mut warning,
                format!(
                    "could not inspect restore token at {}: {err}",
                    path.display()
                ),
            );
            return (None, warning);
        }
    }

    match std::fs::read_to_string(path) {
        Ok(token) => {
            let token = token.trim();
            ((!token.is_empty()).then(|| token.to_owned()), warning)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (None, warning),
        Err(err) => {
            append_warning(
                &mut warning,
                format!("could not read restore token at {}: {err}", path.display()),
            );
            (None, warning)
        }
    }
}

fn store_token(token: &str) -> Result<()> {
    let path = token_path()?;
    let dir = path.parent().context("restore token path has no parent")?;
    create_private_dir(dir)?;
    write_private_file(&path, token)
}

fn create_private_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, Permissions::from_mode(0o700))
        .with_context(|| format!("make {} private", dir.display()))
}

fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().context("private file path has no parent")?;
    let tmp = dir.join(format!(
        ".{}.{}-{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default(),
        std::process::id(),
        monotonic_us()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        file.set_permissions(Permissions::from_mode(0o600))
            .with_context(|| format!("make {} private", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        std::fs::set_permissions(path, Permissions::from_mode(0o600))
            .with_context(|| format!("make {} private", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn append_warning(slot: &mut Option<String>, warning: String) {
    if let Some(existing) = slot {
        existing.push_str("; ");
        existing.push_str(&warning);
    } else {
        *slot = Some(warning);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cosmic-desktop-mcp-eis-{}-{}",
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
    fn restore_token_file_is_private() {
        let temp = TempDir::new();
        let path = temp.0.join("restore-token");

        write_private_file(&path, "secret-token").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "secret-token");
    }

    #[test]
    fn restore_token_directory_is_private() {
        let temp = TempDir::new();
        let dir = temp.0.join("nested/cosmic-desktop-mcp");

        create_private_dir(&dir).unwrap();

        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn load_token_tightens_insecure_permissions() {
        let temp = TempDir::new();
        let path = temp.0.join("restore-token");
        std::fs::write(&path, "secret-token").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        let (token, warning) = load_token_at(&path);

        assert_eq!(token.as_deref(), Some("secret-token"));
        assert!(
            warning
                .as_deref()
                .is_some_and(|warning| warning.contains("tightened to 600"))
        );
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn empty_state_home_falls_back_to_home() {
        let path = state_base_from(Some(OsStr::new("")), Some(OsStr::new("/home/tester"))).unwrap();
        assert_eq!(path, PathBuf::from("/home/tester/.local/state"));
    }

    #[test]
    fn relative_state_locations_are_rejected() {
        assert!(state_base_from(Some(OsStr::new("state")), None).is_err());
        assert!(state_base_from(None, Some(OsStr::new("home"))).is_err());
    }
}

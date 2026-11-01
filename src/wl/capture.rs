//! Screen and toplevel capture over `ext_image_copy_capture_v1` + `wl_shm`.
//!
//! COSMIC does not implement `wlr-screencopy`, and its portal-based
//! `cosmic-screenshot` blocks forever when asked for a non-interactive
//! capture. The staging capture protocol is advertised (global name 17 on this
//! machine) and needs no dialog, so it is the only usable path.
//!
//! Output capture uses `ext_output_image_capture_source_manager_v1`. Window
//! capture uses `ext_foreign_toplevel_image_capture_source_manager_v1` when
//! available, avoiding a full-output copy; callers fall back to an output crop
//! because COSMIC returns direct window sources at logical size and may omit
//! server-side decorations.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::ObjectId,
    event_created_child,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

/// A captured frame, in RGBA8, bottom-up row order corrected.
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub output: OutputInfo,
}

/// A captured image without the output metadata used by whole-output shots.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub index: usize,
    /// Connector name from the `wl_output` v4 `name` event, e.g. `DP-2`.
    /// This is what the EIS region reports as `mapping_id`, so the two can be
    /// matched exactly instead of guessing by physical size.
    pub name: Option<String>,
    pub width: i32,
    pub height: i32,
    pub scale: i32,
}

#[derive(Default)]
struct State {
    outputs: Vec<OutputInfo>,
    output_ids: Vec<ObjectId>,
    pending_outputs: HashMap<ObjectId, PendingOutput>,

    foreign_handles: HashMap<ObjectId, ExtForeignToplevelHandleV1>,
    foreign_identifiers: HashMap<ObjectId, String>,

    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    session_done: bool,
    frame_ready: bool,
    frame_failed: Option<u32>,
    log: Vec<&'static str>,
}

#[derive(Default)]
struct PendingOutput {
    mode: Option<(i32, i32)>,
    scale: Option<i32>,
    name: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let current = flags
                    .into_result()
                    .map(|f| f.contains(wl_output::Mode::Current))
                    .unwrap_or(false);
                let pending = state.pending_outputs.entry(output.id()).or_default();
                if current || pending.mode.is_none() {
                    pending.mode = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => {
                state.pending_outputs.entry(output.id()).or_default().scale = Some(factor);
            }
            wl_output::Event::Name { name } => {
                state.pending_outputs.entry(output.id()).or_default().name = Some(name);
            }
            wl_output::Event::Done => {
                if let Some(pending) = state.pending_outputs.remove(&output.id())
                    && let Some((width, height)) = pending.mode
                {
                    let index = state.outputs.len();
                    state.outputs.push(OutputInfo {
                        index,
                        name: pending.name,
                        width,
                        height,
                        scale: pending.scale.unwrap_or(1),
                    });
                    state.output_ids.push(output.id());
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCaptureSourceV1,
        _event: <ExtImageCaptureSourceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtOutputImageCaptureSourceManagerV1,
        _event: <ExtOutputImageCaptureSourceManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtForeignToplevelImageCaptureSourceManagerV1,
        _event: <ExtForeignToplevelImageCaptureSourceManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.foreign_handles.insert(toplevel.id(), toplevel);
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                state.foreign_identifiers.insert(proxy.id(), identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.foreign_handles.remove(&proxy.id());
                state.foreign_identifiers.remove(&proxy.id());
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.log.push("buffer_size");
                state.size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                state.log.push("shm_format");
                if let Ok(format) = format.into_result() {
                    state.formats.push(format);
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                state.log.push("session_done");
                state.session_done = true;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.log.push("session_stopped");
                state.frame_failed.get_or_insert(0);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform { .. } => {
                state.log.push("frame_transform");
            }
            ext_image_copy_capture_frame_v1::Event::Damage { .. } => {
                state.log.push("frame_damage");
            }
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.log.push("frame_ready");
                state.frame_ready = true;
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.log.push("frame_failed");
                let code = reason.into_result().map(|r| r as u32).unwrap_or(u32::MAX);
                state.frame_failed = Some(code);
            }
            _ => {}
        }
    }
}

/// Lists the outputs the compositor advertises, in registry order.
pub fn outputs() -> Result<Vec<OutputInfo>> {
    let (_conn, _queue, state) = connect()?;
    Ok(state.outputs)
}

fn connect() -> Result<(Connection, wayland_client::EventQueue<State>, State)> {
    let conn = super::connect()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    let registry = globals.registry().clone();
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            let _: wl_output::WlOutput = registry.bind(global.name, global.version.min(4), &qh, ());
        }
    }

    let mut state = State::default();
    queue
        .roundtrip(&mut state)
        .context("roundtrip for outputs")?;
    Ok((conn, queue, state))
}

/// Captures `output_index`. Rows are returned top-down.
pub fn capture(output_index: usize) -> Result<Shot> {
    let conn = super::connect()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();
    let mut state = State::default();

    let registry = globals.registry().clone();
    let mut output_proxies = Vec::new();
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            let proxy: wl_output::WlOutput =
                registry.bind(global.name, global.version.min(4), &qh, ());
            output_proxies.push(proxy);
        }
    }
    queue
        .roundtrip(&mut state)
        .context("roundtrip for outputs")?;

    if state.outputs.is_empty() {
        bail!("compositor advertised no wl_output");
    }
    let target = state
        .outputs
        .get(output_index)
        .with_context(|| {
            format!(
                "output index {output_index} out of range ({} outputs)",
                state.outputs.len()
            )
        })?
        .clone();
    let output_id = state
        .output_ids
        .get(output_index)
        .context("output id index out of range")?;
    let output = output_proxies
        .iter()
        .find(|proxy| proxy.id() == *output_id)
        .context("output proxy not found for captured output")?
        .clone();

    let source_mgr: ExtOutputImageCaptureSourceManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("bind ext_output_image_capture_source_manager_v1")?;
    let source = source_mgr.create_source(&output, &qh, ());
    let frame = capture_source(&conn, &globals, &mut queue, &mut state, source)?;

    Ok(Shot {
        width: frame.width,
        height: frame.height,
        rgba: frame.rgba,
        output: target,
    })
}

/// Captures a foreign toplevel directly, without first copying its output.
///
/// `None` means the compositor does not advertise the window capture source
/// protocol, so the caller should use [`capture`] plus a crop. The source
/// image may contain the whole toplevel even when part of it is off-screen;
/// callers must fit it back into the visible crop before using its pixels as
/// window-image coordinates.
pub fn capture_window(identifier: &str) -> Result<Option<Frame>> {
    let conn = super::connect()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    let Ok(source_mgr) =
        globals.bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())
    else {
        return Ok(None);
    };
    let Ok(_list) = globals.bind::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ()) else {
        return Ok(None);
    };

    let mut state = State::default();
    for _ in 0..3 {
        queue
            .roundtrip(&mut state)
            .context("roundtrip for window capture handles")?;
    }

    let target = state
        .foreign_identifiers
        .iter()
        .find_map(|(id, found)| (found == identifier).then_some(id))
        .and_then(|id| state.foreign_handles.get(id))
        .with_context(|| format!("window {identifier:?} was not available for direct capture"))?
        .clone();

    let source = source_mgr.create_source(&target, &qh, ());
    capture_source(&conn, &globals, &mut queue, &mut state, source).map(Some)
}

fn capture_source(
    conn: &Connection,
    globals: &GlobalList,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    source: ExtImageCaptureSourceV1,
) -> Result<Frame> {
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("bind wl_shm")?;
    let capture_mgr: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("bind ext_image_copy_capture_manager_v1")?;

    let session = capture_mgr.create_session(&source, Options::empty(), &qh, ());

    wait(conn, queue, state, |s| s.session_done && s.size.is_some())
        .context("waiting for capture session parameters")?;

    if let Some(code) = state.frame_failed.take() {
        bail!("capture session was stopped (reason {code})");
    }

    let (width, height) = state.size.context("session sent no buffer_size")?;
    let format = pick_format(&state.formats)?;
    let stride = width * 4;
    let len = (stride as usize) * (height as usize);

    let fd = memfd(len)?;
    let pool = shm.create_pool(fd.as_fd(), len as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        &qh,
        (),
    );

    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    // Required on the first capture of a buffer: the compositor otherwise has
    // no reason to consider the contents stale.
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();
    conn.flush().context("flush capture request")?;

    wait(conn, queue, state, |s| {
        s.frame_ready || s.frame_failed.is_some()
    })
    .context("waiting for captured frame")?;

    if let Some(code) = state.frame_failed {
        bail!("compositor failed the capture (reason {code})");
    }

    let rgba = read_frame(fd.as_fd(), len, width, height, format)?;

    frame.destroy();
    buffer.destroy();
    pool.destroy();
    session.destroy();
    source.destroy();

    Ok(Frame {
        width,
        height,
        rgba,
    })
}

fn pick_format(formats: &[wl_shm::Format]) -> Result<wl_shm::Format> {
    // COSMIC offers ABGR/XBGR8888 (byte order R,G,B,A) rather than the more
    // common ARGB/XRGB. Prefer what it actually advertises.
    for wanted in [
        wl_shm::Format::Abgr8888,
        wl_shm::Format::Xbgr8888,
        wl_shm::Format::Argb8888,
        wl_shm::Format::Xrgb8888,
    ] {
        if formats.contains(&wanted) {
            return Ok(wanted);
        }
    }
    bail!(
        "compositor offered no usable shm format (got {:?})",
        formats
    )
}

/// Pumps the queue until `done` holds. Bounded so a silent compositor cannot
/// hang the MCP server forever.
fn wait(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    done: impl Fn(&State) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(state) {
        if Instant::now() > deadline {
            bail!(
                "timed out after 5s; events seen: {:?} — a session lock screen \
                 commonly produces exactly this silence, so check whether the \
                 screen is locked before blaming the tool",
                state.log
            );
        }
        queue
            .roundtrip(state)
            .context("roundtrip while waiting for capture")?;
        conn.flush().ok();
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn memfd(len: usize) -> Result<OwnedFd> {
    let name = CString::new("cosmic-desktop-mcp").context("CString")?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let rc = unsafe { libc::ftruncate(fd, len as libc::off_t) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("ftruncate");
    }
    Ok(owned)
}

fn read_frame(
    fd: std::os::fd::BorrowedFd<'_>,
    len: usize,
    width: u32,
    height: u32,
    format: wl_shm::Format,
) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;

    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error()).context("mmap captured buffer");
    }

    let src = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    let mut rgba = Vec::with_capacity(len);
    // Little-endian 32-bit words: ARGB8888 is B,G,R,A in memory, ABGR8888 is
    // R,G,B,A. Handle both so the tool works on other compositors too.
    match format {
        wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => {
            let opaque = format == wl_shm::Format::Xbgr8888;
            for pixel in src.as_chunks::<4>().0 {
                let alpha = if opaque { 0xff } else { pixel[3] };
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], alpha]);
            }
        }
        _ => {
            let opaque = format == wl_shm::Format::Xrgb8888;
            for pixel in src.as_chunks::<4>().0 {
                let alpha = if opaque { 0xff } else { pixel[3] };
                rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
            }
        }
    }

    unsafe {
        libc::munmap(ptr, len);
    }
    let _ = (width, height);
    Ok(rgba)
}

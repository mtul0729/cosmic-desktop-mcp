//! Window enumeration and control.
//!
//! Two protocols are needed because each carries half of what is useful:
//!
//! * `ext_foreign_toplevel_list_v1` (standard, wayland-protocols staging)
//!   gives `identifier`, `title` and `app_id` — the same objects `lswt` and
//!   any other ext-foreign-toplevel client sees.
//! * `zcosmic_toplevel_info_v1` (COSMIC, v2+) gives the states and, the
//!   reason this module exists, `geometry`: the toplevel's position and size
//!   **relative to an output**. Standard protocols deliberately omit window
//!   placement, and `wlr-foreign-toplevel-management` (the usual answer) is
//!   not implemented by cosmic-comp.
//! * `zcosmic_toplevel_manager_v1` (COSMIC) turns that list into something
//!   actionable: activate / close / maximize / minimize / fullscreen.
//!   `activate` matters beyond convenience — an EIS click does **not** move
//!   keyboard focus on COSMIC, so clicking a window and then typing lands in
//!   whatever had focus before. Activating first is the fix.
//!
//! The v2 split matters: binding version 1 of `zcosmic_toplevel_info_v1`
//! yields toplevel handles from a `toplevel` event but no geometry at all
//! (geometry is `since="2"`). Version 2 removes that event and instead asks
//! the client to call `get_cosmic_toplevel` for each `ext_foreign_toplevel_handle_v1`
//! it already has, which is exactly the join key this module needs.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
    zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
};
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::{
    self, ZcosmicToplevelManagerV1,
};
use serde::Serialize;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, backend::ObjectId, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_output, wl_registry, wl_seat},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};

/// A rectangle in compositor (logical) coordinates, relative to an output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Window {
    pub identifier: String,
    pub title: String,
    pub app_id: String,
    /// `maximized`, `minimized`, `activated`, `fullscreen`, `sticky`.
    pub state: Vec<String>,
    /// Connector name of the output the geometry is relative to.
    pub output: Option<String>,
    /// Placement in logical coordinates. `None` until the compositor has sent
    /// a `geometry` event.
    pub logical: Option<Rect>,
}

/// What `action` may ask the compositor to do. The compositor is always free
/// to ignore a request; `capabilities` says which ones it honours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Activate,
    Close,
    Maximize,
    Unmaximize,
    Minimize,
    Unminimize,
    Fullscreen,
    Unfullscreen,
}

impl Action {
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "activate" => Self::Activate,
            "close" => Self::Close,
            "maximize" => Self::Maximize,
            "unmaximize" => Self::Unmaximize,
            "minimize" => Self::Minimize,
            "unminimize" => Self::Unminimize,
            "fullscreen" => Self::Fullscreen,
            "unfullscreen" => Self::Unfullscreen,
            other => bail!(
                "unknown window action {other:?}; expected one of activate, close, maximize, \
                 unmaximize, minimize, unminimize, fullscreen, unfullscreen"
            ),
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Close => "close",
            Self::Maximize => "maximize",
            Self::Unmaximize => "unmaximize",
            Self::Minimize => "minimize",
            Self::Unminimize => "unminimize",
            Self::Fullscreen => "fullscreen",
            Self::Unfullscreen => "unfullscreen",
        }
    }
}

#[derive(Default)]
struct State {
    info: Option<ZcosmicToplevelInfoV1>,
    /// `wl_output` object id -> connector name (`DP-2`, `eDP-1`).
    outputs: HashMap<ObjectId, String>,
    /// cosmic handle id -> window.
    windows: HashMap<ObjectId, Window>,
    /// ext handle id -> cosmic handle id, and back.
    link: HashMap<ObjectId, ObjectId>,
    owner: HashMap<ObjectId, ObjectId>,
    /// ext handle id -> the proxy, so requests can be addressed to it.
    ext_handles: HashMap<ObjectId, ExtForeignToplevelHandleV1>,
    /// cosmic handle id -> handles, so `action` can send requests.
    cosmic_handles: HashMap<ObjectId, ZcosmicToplevelHandleV1>,
    /// cosmic handle id -> (output id, rect) as reported; resolved to a name
    /// at the end because `wl_output.name` may still be in flight.
    geometry: HashMap<ObjectId, Vec<(ObjectId, Rect)>>,
    capabilities: Vec<u32>,
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
        if let wl_output::Event::Name { name } = event {
            state.outputs.insert(output.id(), name);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
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
        qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            // Ask for the COSMIC extension object for this handle *now*, so
            // its geometry arrives in the same roundtrip batch as the title.
            // Every window is keyed by its cosmic handle; `link` carries the
            // join from the ext handle the title/app_id events arrive on.
            state.ext_handles.insert(toplevel.id(), toplevel.clone());
            match state.info.as_ref() {
                Some(info) => {
                    let cosmic = info.get_cosmic_toplevel(&toplevel, qh, ());
                    state.link.insert(toplevel.id(), cosmic.id());
                    state.owner.insert(cosmic.id(), toplevel.id());
                    state.cosmic_handles.insert(cosmic.id(), cosmic.clone());
                    state.windows.entry(cosmic.id()).or_default();
                }
                None => {
                    state.windows.entry(toplevel.id()).or_default();
                }
            }
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
        let Some(cosmic_id) = state.link.get(&proxy.id()).cloned() else {
            return;
        };
        let entry = state.windows.entry(cosmic_id.clone()).or_default();
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                entry.identifier = identifier;
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => entry.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => entry.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.windows.remove(&cosmic_id);
                state.geometry.remove(&cosmic_id);
                state.cosmic_handles.remove(&cosmic_id);
                state.owner.remove(&cosmic_id);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // `Toplevel` is v1-only; v2 clients create handles themselves.
        if let zcosmic_toplevel_info_v1::Event::Toplevel { toplevel } = event {
            state.windows.entry(toplevel.id()).or_default();
            state.cosmic_handles.insert(toplevel.id(), toplevel.clone());
        }
    }

    event_created_child!(State, ZcosmicToplevelInfoV1, [
        zcosmic_toplevel_info_v1::EVT_TOPLEVEL_OPCODE => (ZcosmicToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZcosmicToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = proxy.id();
        match event {
            zcosmic_toplevel_handle_v1::Event::State { state: raw } => {
                let entry = state.windows.entry(id.clone()).or_default();
                entry.state = state_names(&raw);
            }
            zcosmic_toplevel_handle_v1::Event::Geometry {
                output,
                x,
                y,
                width,
                height,
            } => {
                // A window can be visible on several outputs and reports one
                // geometry per output. Keep them all; the caller picks.
                state
                    .geometry
                    .entry(id.clone())
                    .or_default()
                    .push((output.id(), Rect { x, y, width, height }));
            }
            zcosmic_toplevel_handle_v1::Event::Closed => {
                state.windows.remove(&id);
                state.geometry.remove(&id);
                state.cosmic_handles.remove(&id);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZcosmicToplevelManagerV1,
        event: zcosmic_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zcosmic_toplevel_manager_v1::Event::Capabilities { capabilities } = event {
            state.capabilities = capabilities
                .chunks_exact(4)
                .map(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap()))
                .collect();
        }
    }
}

/// Decodes the `state` array: a list of native-endian u32 enum values.
fn state_names(raw: &[u8]) -> Vec<String> {
    raw.chunks_exact(4)
        .map(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap()))
        .map(|value| match value {
            0 => "maximized",
            1 => "minimized",
            2 => "activated",
            3 => "fullscreen",
            4 => "sticky",
            _ => "unknown",
        })
        .map(str::to_string)
        .collect()
}

/// A live registry snapshot: everything bound, every window already reported.
pub struct Snapshot {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    list: ExtForeignToplevelListV1,
    manager: Option<ZcosmicToplevelManagerV1>,
    seat: Option<wl_seat::WlSeat>,
}

impl Snapshot {
    /// The windows, with geometry resolved to output names.
    pub fn windows(&self) -> Vec<Window> {
        let mut windows: Vec<Window> = self
            .state
            .windows
            .iter()
            .map(|(id, window)| {
                let mut window = window.clone();
                if let Some(reported) = self.state.geometry.get(id) {
                    // Prefer a named output; several geometries mean the
                    // window spans outputs, and the first is the one it is
                    // anchored to.
                    if let Some((output_id, rect)) = reported
                        .iter()
                        .find(|(output_id, _)| self.state.outputs.contains_key(output_id))
                        .or_else(|| reported.first())
                    {
                        window.output = self.state.outputs.get(output_id).cloned();
                        window.logical = Some(*rect);
                    }
                }
                window
            })
            .filter(|window| !window.identifier.is_empty())
            .collect();
        windows.sort_by(|a, b| a.app_id.cmp(&b.app_id).then(a.title.cmp(&b.title)));
        windows
    }

    /// Resolves a `window` argument to exactly one window.
    pub fn find(&self, needle: &str) -> Result<Window> {
        find(&self.windows(), needle)
    }

    /// Asks the compositor to act on a window. Returns a human-readable line
    /// describing what was requested, since the compositor may ignore it.
    pub fn act(&mut self, needle: &str, action: Action) -> Result<String> {
        let target = self.find(needle)?;
        let manager = self
            .manager
            .as_ref()
            .context("this compositor has no zcosmic_toplevel_manager_v1")?;

        let cosmic_id = self
            .state
            .windows
            .iter()
            .find(|(_, window)| window.identifier == target.identifier)
            .map(|(id, _)| id.clone())
            .with_context(|| format!("window {:?} disappeared", target.title))?;
        let handle = self
            .state
            .cosmic_handles
            .get(&cosmic_id)
            .cloned()
            .context("no cosmic handle for this window")?;

        match action {
            Action::Activate => {
                let seat = self
                    .seat
                    .as_ref()
                    .context("activate needs wl_seat, which this compositor did not advertise")?;
                manager.activate(&handle, seat);
            }
            Action::Close => manager.close(&handle),
            Action::Maximize => manager.set_maximized(&handle),
            Action::Unmaximize => manager.unset_maximized(&handle),
            Action::Minimize => manager.set_minimized(&handle),
            Action::Unminimize => manager.unset_minimized(&handle),
            Action::Fullscreen => manager.set_fullscreen(&handle, None),
            Action::Unfullscreen => manager.unset_fullscreen(&handle),
        }

        // Give the roundtrip a chance to carry the request and the resulting
        // state event; the compositor is allowed to ignore it either way.
        self.conn.flush().context("flush window action")?;
        self.queue
            .roundtrip(&mut self.state)
            .context("roundtrip after window action")?;

        Ok(format!(
            "{} {:?} ({})",
            action.as_str(),
            target.title,
            target.app_id
        ))
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.list.destroy();
    }
}

fn connect(with_manager: bool) -> Result<Snapshot> {
    let conn = Connection::connect_to_env().context("connect to WAYLAND_DISPLAY")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    // v2 is the minimum that carries geometry; binding below v3 keeps the
    // ext_workspace_* events out of the picture until something uses them.
    let info: ZcosmicToplevelInfoV1 = globals.bind(&qh, 2..=3, ()).context(
        "bind zcosmic_toplevel_info_v1 v2 (does this compositor have the COSMIC protocols?)",
    )?;

    let list: ExtForeignToplevelListV1 = globals
        .bind(&qh, 1..=1, ())
        .context("bind ext_foreign_toplevel_list_v1 (compositor does not support it)")?;

    let manager = if with_manager {
        globals
            .bind::<ZcosmicToplevelManagerV1, _, _>(&qh, 1..=4, ())
            .ok()
    } else {
        None
    };
    let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ()).ok();

    let registry = globals.registry().clone();
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            let _: wl_output::WlOutput = registry.bind(global.name, global.version.min(4), &qh, ());
        }
    }

    let mut state = State {
        info: Some(info),
        ..State::default()
    };
    // Titles and geometry trail the handles by one or two batches: the
    // `get_cosmic_toplevel` requests only go out while dispatching the
    // Toplevel events of the first roundtrip.
    for _ in 0..4 {
        queue.roundtrip(&mut state).context("roundtrip for toplevels")?;
    }
    // The initial state of a cosmic handle (states, geometry) is pushed from
    // inside the compositor's frame-commit path, so an idle compositor needs
    // one rendered frame after `get_cosmic_toplevel` before it answers. Poll
    // briefly instead of hoping the four roundtrips above straddle a frame;
    // bail out as soon as any window has reported placement.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
    while std::time::Instant::now() < deadline
        && state
            .windows
            .values()
            .all(|window| window.state.is_empty() && window.logical.is_none())
    {
        std::thread::sleep(std::time::Duration::from_millis(60));
        queue.roundtrip(&mut state).context("roundtrip for toplevels")?;
    }

    Ok(Snapshot {
        conn,
        queue,
        state,
        list,
        manager,
        seat,
    })
}

/// Resolves a `window` argument to exactly one window.
///
/// Accepts the protocol `identifier` verbatim, or an `app_id`, or a title —
/// the last two only when they match exactly one window, because silently
/// picking the first of several identical terminals is how clicks land in the
/// wrong place.
pub fn find(windows: &[Window], needle: &str) -> Result<Window> {
    if let Some(found) = windows.iter().find(|w| w.identifier == needle) {
        return Ok(found.clone());
    }

    let by_app: Vec<&Window> = windows.iter().filter(|w| w.app_id == needle).collect();
    if by_app.len() == 1 {
        return Ok(by_app[0].clone());
    }

    let by_title: Vec<&Window> = windows
        .iter()
        .filter(|w| w.title.eq_ignore_ascii_case(needle))
        .collect();
    match by_title.len() {
        1 => return Ok(by_title[0].clone()),
        0 => {}
        count => bail!("{needle:?} matches {count} windows by title; use the identifier instead"),
    }

    let partial: Vec<&Window> = windows
        .iter()
        .filter(|w| w.title.contains(needle))
        .collect();
    match partial.len() {
        1 => Ok(partial[0].clone()),
        0 => bail!(
            "no window matches {needle:?}; open windows: {}",
            windows
                .iter()
                .map(|w| format!("{:?} ({})", w.title, w.app_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        count => bail!("{needle:?} matches {count} windows by title; use the identifier instead"),
    }
}

/// Enumerates windows. Never sends anything but reads.
pub fn list() -> Result<Vec<Window>> {
    Ok(connect(false)?.windows())
}

/// Performs `action` on the window matching `needle`.
pub fn act(needle: &str, action: Action) -> Result<String> {
    connect(true)?.act(needle, action)
}

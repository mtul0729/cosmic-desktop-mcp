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
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::ObjectId,
    event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_output, wl_registry, wl_seat},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State as WsState},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

/// A rectangle in compositor (logical) coordinates, relative to an output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// One workspace a window is on, joined from `ext_workspace_enter` (since
/// zcosmic_toplevel_info_v1 v3) and `ext_workspace_manager_v1`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct WindowWorkspace {
    /// Index of the workspace group (one per output), in compositor order.
    pub group: usize,
    pub name: Option<String>,
    /// Grid position as reported; COSMIC sends one number.
    pub coordinates: Vec<u32>,
    /// This workspace is the one the group currently shows.
    pub active: bool,
    pub hidden: bool,
    /// Connector names of the group's outputs, when the compositor reported
    /// them. Empty is "unknown", not "no output".
    pub outputs: Vec<String>,
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
    /// Workspaces the window is on. `None` means the compositor did not expose
    /// membership (zcosmic_toplevel_info_v1 below v3, or no
    /// ext_workspace_manager_v1), not that it is on none.
    pub workspaces: Option<Vec<WindowWorkspace>>,
    /// Whether one of those workspaces is the active one. `None` for the same
    /// reason as `workspaces`.
    pub on_active_workspace: Option<bool>,
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

pub struct ActionOutcome {
    pub window: Window,
    pub summary: String,
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
    /// Version negotiated for zcosmic_toplevel_info_v1. `ext_workspace_enter`
    /// only exists at v3.
    info_version: u32,
    /// Whether ext_workspace_manager_v1 was bound. Without it the workspace
    /// object ids a window reports cannot be named.
    workspace_protocol: bool,
    /// Group handles in compositor order; the index is the group number the
    /// tools report.
    group_order: Vec<ObjectId>,
    /// Workspace id -> its group id.
    workspace_group: HashMap<ObjectId, ObjectId>,
    /// Group id -> the workspaces that entered it.
    group_members: HashMap<ObjectId, Vec<ObjectId>>,
    /// Group id -> the wl_output ids that entered it.
    group_outputs: HashMap<ObjectId, Vec<ObjectId>>,
    /// Workspace id -> its properties.
    workspaces: HashMap<ObjectId, WindowWorkspace>,
    /// cosmic handle id -> workspace ids from ext_workspace_enter/leave.
    window_workspaces: HashMap<ObjectId, Vec<ObjectId>>,
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

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } = event {
            let id = workspace_group.id();
            if !state.group_order.contains(&id) {
                state.group_order.push(id);
            }
        }
    }

    event_created_child!(State, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                let outputs = state.group_outputs.entry(proxy.id()).or_default();
                if !outputs.contains(&output.id()) {
                    outputs.push(output.id());
                }
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                if let Some(outputs) = state.group_outputs.get_mut(&proxy.id()) {
                    outputs.retain(|id| *id != output.id());
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                let members = state.group_members.entry(proxy.id()).or_default();
                if !members.contains(&workspace.id()) {
                    members.push(workspace.id());
                }
                state.workspace_group.insert(workspace.id(), proxy.id());
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                if let Some(members) = state.group_members.get_mut(&proxy.id()) {
                    members.retain(|id| *id != workspace.id());
                }
                state.workspace_group.remove(&workspace.id());
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                state.group_members.remove(&proxy.id());
                state.group_outputs.remove(&proxy.id());
                state.group_order.retain(|id| *id != proxy.id());
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let entry = state.workspaces.entry(proxy.id()).or_default();
        match event {
            ext_workspace_handle_v1::Event::Name { name } => entry.name = Some(name),
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                entry.coordinates = coordinates
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| u32::from_ne_bytes(*chunk))
                    .collect();
            }
            ext_workspace_handle_v1::Event::State { state: flags } => {
                let bits = match flags {
                    WEnum::Value(flags) => flags.bits(),
                    WEnum::Unknown(raw) => raw,
                };
                entry.active = bits & WsState::Active.bits() != 0;
                entry.hidden = bits & WsState::Hidden.bits() != 0;
            }
            ext_workspace_handle_v1::Event::Removed => {
                state.workspaces.remove(&proxy.id());
            }
            _ => {}
        }
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
                // geometry per output. Replace the entry for that output:
                // repeated events describe the same placement changing, not
                // additional windows.
                upsert_geometry(
                    state.geometry.entry(id.clone()).or_default(),
                    output.id(),
                    Rect {
                        x,
                        y,
                        width,
                        height,
                    },
                );
            }
            // v3 replaced the zcosmic_workspace_* events with the standard
            // workspace objects, which is what the workspace manager can name.
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceEnter { workspace } => {
                let entered = state.window_workspaces.entry(id.clone()).or_default();
                if !entered.contains(&workspace.id()) {
                    entered.push(workspace.id());
                }
            }
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceLeave { workspace } => {
                if let Some(entered) = state.window_workspaces.get_mut(&id) {
                    entered.retain(|other| *other != workspace.id());
                }
            }
            zcosmic_toplevel_handle_v1::Event::Closed => {
                state.windows.remove(&id);
                state.geometry.remove(&id);
                state.cosmic_handles.remove(&id);
                state.window_workspaces.remove(&id);
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
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| u32::from_ne_bytes(*chunk))
                .collect();
        }
    }
}

/// Decodes the `state` array: a list of native-endian u32 enum values.
fn state_names(raw: &[u8]) -> Vec<String> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_ne_bytes(*chunk))
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
    workspace_manager: Option<ExtWorkspaceManagerV1>,
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
                if self.state.info_version >= 3
                    && self.state.workspace_protocol
                    && let Some((workspaces, on_active)) = self.workspaces_of(id)
                {
                    window.workspaces = Some(workspaces);
                    window.on_active_workspace = Some(on_active);
                }
                window
            })
            .filter(|window| !window.identifier.is_empty())
            .collect();
        windows.sort_by(|a, b| a.app_id.cmp(&b.app_id).then(a.title.cmp(&b.title)));
        windows
    }

    /// Joins a window's `ext_workspace_enter` ids to the workspace manager's
    /// names, groups and active flags.
    ///
    /// Returns `None` when any id cannot be resolved: a partial list would be
    /// worse than "unknown", because a window on two workspaces would look like
    /// it is on one.
    fn workspaces_of(&self, window: &ObjectId) -> Option<(Vec<WindowWorkspace>, bool)> {
        let ids = self.state.window_workspaces.get(window)?;
        if ids.is_empty() {
            return None;
        }
        let mut views = Vec::with_capacity(ids.len());
        for workspace_id in ids {
            let workspace = self.state.workspaces.get(workspace_id)?;
            let group_id = self.state.workspace_group.get(workspace_id)?;
            let group = self
                .state
                .group_order
                .iter()
                .position(|candidate| candidate == group_id)?;
            let mut view = workspace.clone();
            view.group = group;
            view.outputs = self
                .state
                .group_outputs
                .get(group_id)
                .map(|outputs| {
                    outputs
                        .iter()
                        .filter_map(|output| self.state.outputs.get(output).cloned())
                        .collect()
                })
                .unwrap_or_default();
            views.push(view);
        }
        let on_active = views.iter().any(|workspace| workspace.active);
        Some((views, on_active))
    }

    /// Resolves a `window` argument to exactly one window.
    pub fn find(&self, needle: &str) -> Result<Window> {
        find(&self.windows(), needle)
    }

    /// Asks the compositor to act on a window. Returns a human-readable line
    /// describing what was requested, since the compositor may ignore it.
    pub fn act(&mut self, needle: &str, action: Action) -> Result<ActionOutcome> {
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

        Ok(ActionOutcome {
            summary: format!("{} {:?} ({})", action.as_str(), target.title, target.app_id),
            window: target,
        })
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.list.destroy();
        if let Some(manager) = self.workspace_manager.as_ref() {
            manager.stop();
        }
    }
}

fn connect(with_manager: bool) -> Result<Snapshot> {
    let conn = super::connect()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    // v2 is the minimum that carries geometry; binding below v3 keeps the
    // ext_workspace_* events out of the picture until something uses them.
    let info: ZcosmicToplevelInfoV1 = globals.bind(&qh, 2..=3, ()).context(
        "bind zcosmic_toplevel_info_v1 v2 (does this compositor have the COSMIC protocols?)",
    )?;
    let info_version = info.version();

    let list: ExtForeignToplevelListV1 = globals
        .bind(&qh, 1..=1, ())
        .context("bind ext_foreign_toplevel_list_v1 (compositor does not support it)")?;

    // Workspace membership is optional: v3 of the toplevel info protocol
    // reports it as ext_workspace_handle_v1 objects, and only this manager can
    // turn those into names and active flags. Without either, the fields stay
    // null instead of being guessed.
    let workspace_manager = globals
        .bind::<ExtWorkspaceManagerV1, _, _>(&qh, 1..=1, ())
        .ok();

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
        info_version,
        workspace_protocol: workspace_manager.is_some(),
        ..State::default()
    };
    // Titles and geometry trail the handles by one or two batches: the
    // `get_cosmic_toplevel` requests only go out while dispatching the
    // Toplevel events of the first roundtrip.
    for _ in 0..4 {
        queue
            .roundtrip(&mut state)
            .context("roundtrip for toplevels")?;
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
        queue
            .roundtrip(&mut state)
            .context("roundtrip for toplevels")?;
    }

    Ok(Snapshot {
        conn,
        queue,
        state,
        list,
        manager,
        workspace_manager,
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
    match by_app.len() {
        0 => {}
        1 => return Ok(by_app[0].clone()),
        count => {
            bail!("{needle:?} matches {count} windows by app_id; use the identifier instead")
        }
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
pub fn act(needle: &str, action: Action) -> Result<ActionOutcome> {
    connect(true)?.act(needle, action)
}

fn upsert_geometry<K: PartialEq>(reported: &mut Vec<(K, Rect)>, output: K, rect: Rect) {
    if let Some((_, existing)) = reported
        .iter_mut()
        .find(|(candidate, _)| *candidate == output)
    {
        *existing = rect;
    } else {
        reported.push((output, rect));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(identifier: &str, app_id: &str, title: &str) -> Window {
        Window {
            identifier: identifier.to_string(),
            app_id: app_id.to_string(),
            title: title.to_string(),
            ..Window::default()
        }
    }

    #[test]
    fn ambiguous_app_ids_do_not_fall_through_to_titles() {
        let windows = vec![
            window("one", "kitty", "kitty"),
            window("two", "kitty", "notes"),
        ];

        let err = find(&windows, "kitty").unwrap_err().to_string();

        assert!(err.contains("2 windows by app_id"), "{err}");
    }

    #[test]
    fn repeated_geometry_replaces_the_same_output() {
        let first = Rect {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };
        let moved = Rect {
            x: 50,
            y: 60,
            width: 640,
            height: 480,
        };
        let mut reported = vec![(7u32, first)];

        upsert_geometry(&mut reported, 7, moved);

        assert_eq!(reported, vec![(7, moved)]);
    }
}

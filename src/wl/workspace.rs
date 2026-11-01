//! Workspace enumeration and activation over `ext_workspace_manager_v1`.
//!
//! COSMIC advertises the staging protocol (global 51 here), so no vendor XML is
//! needed. Two COSMIC-specific facts shape this module:
//!
//! - **It never sends the `id` event**, although the protocol calls it
//!   mandatory. A workspace can therefore only be identified by its group, its
//!   name and its grid position.
//! - **Workspaces are per output**: each monitor gets its own group, so two
//!   screens both have a workspace named "1". Groups are reported by index
//!   because the `wl_output` proxies handed out by `workspace_enter` carry no
//!   connector name here.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    backend::ObjectId,
    event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{
        self, ExtWorkspaceHandleV1, State as WsState, WorkspaceCapabilities as WsCapability,
    },
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Workspace {
    /// Index of the workspace group (one per output), in compositor order.
    pub group: usize,
    pub name: Option<String>,
    /// Grid position as reported by the compositor; COSMIC sends one number.
    pub coordinates: Vec<u32>,
    pub active: bool,
    pub urgent: bool,
    pub hidden: bool,
    pub can_activate: bool,
    #[serde(skip)]
    handle: Option<ExtWorkspaceHandleV1>,
}

#[derive(Default)]
struct State {
    workspaces: HashMap<ObjectId, Workspace>,
    groups: Vec<ObjectId>,
    members: HashMap<ObjectId, Vec<ObjectId>>,
    done: bool,
    finished: bool,
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

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                let id = workspace_group.id();
                if !state.groups.contains(&id) {
                    state.groups.push(id);
                }
            }
            ext_workspace_manager_v1::Event::Done => state.done = true,
            ext_workspace_manager_v1::Event::Finished => state.finished = true,
            _ => {}
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
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                let members = state.members.entry(proxy.id()).or_default();
                if !members.contains(&workspace.id()) {
                    members.push(workspace.id());
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                if let Some(members) = state.members.get_mut(&proxy.id()) {
                    members.retain(|id| *id != workspace.id());
                }
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                state.members.remove(&proxy.id());
                state.groups.retain(|id| *id != proxy.id());
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
        entry.handle.get_or_insert_with(|| proxy.clone());
        match event {
            // COSMIC does not send `id`, so it is deliberately not handled.
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
                // A combination of flags is not a single variant, so the enum
                // arrives as Unknown carrying the raw bits.
                let bits = match flags {
                    WEnum::Value(flags) => flags.bits(),
                    WEnum::Unknown(raw) => raw,
                };
                entry.active = bits & WsState::Active.bits() != 0;
                entry.urgent = bits & WsState::Urgent.bits() != 0;
                entry.hidden = bits & WsState::Hidden.bits() != 0;
            }
            ext_workspace_handle_v1::Event::Capabilities { capabilities } => {
                let bits = match capabilities {
                    WEnum::Value(flags) => flags.bits(),
                    WEnum::Unknown(raw) => raw,
                };
                entry.can_activate = bits & WsCapability::Activate.bits() != 0;
            }
            ext_workspace_handle_v1::Event::Removed => {
                state.workspaces.remove(&proxy.id());
            }
            _ => {}
        }
    }
}

type Session = (
    Connection,
    wayland_client::EventQueue<State>,
    ExtWorkspaceManagerV1,
    State,
);

fn connect() -> Result<Session> {
    let conn = super::connect()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();
    let manager: ExtWorkspaceManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("bind ext_workspace_manager_v1 (compositor does not support it)")?;

    let mut state = State::default();
    // Groups, workspaces and their properties arrive in separate batches.
    for _ in 0..4 {
        queue
            .roundtrip(&mut state)
            .context("roundtrip for workspaces")?;
    }
    Ok((conn, queue, manager, state))
}

fn collect(state: State) -> Vec<Workspace> {
    let mut out = Vec::new();
    for (index, group) in state.groups.iter().enumerate() {
        for workspace_id in state.members.get(group).cloned().unwrap_or_default() {
            if let Some(mut workspace) = state.workspaces.get(&workspace_id).cloned() {
                workspace.group = index;
                out.push(workspace);
            }
        }
    }
    out.retain(|workspace| workspace.handle.is_some());
    out.sort_by(|a, b| {
        a.group
            .cmp(&b.group)
            .then(a.coordinates.cmp(&b.coordinates))
    });
    out
}

pub fn list() -> Result<Vec<Workspace>> {
    let (_conn, _queue, manager, state) = connect()?;
    manager.stop();
    Ok(collect(state))
}

/// Activates the workspace matching `target` — its name (`"2"`) or its grid
/// position (`"2,0"`). When several outputs have a workspace by that name,
/// `group` picks one; with no group and an ambiguous match this refuses
/// instead of guessing which monitor the caller meant.
pub fn activate(target: &str, group: Option<usize>) -> Result<Vec<Workspace>> {
    let (_conn, mut queue, manager, mut state) = connect()?;

    let wanted = target.trim();
    if wanted.is_empty() {
        bail!("workspace selector must not be empty");
    }
    let matches = |workspace: &Workspace| {
        workspace.name.as_deref() == Some(wanted)
            || coordinates_key(&workspace.coordinates) == wanted
    };

    let before = collect(state.clone_shallow());
    let candidates: Vec<&Workspace> = before
        .iter()
        .filter(|workspace| matches(workspace))
        .filter(|workspace| group.is_none_or(|index| workspace.group == index))
        .collect();

    let handle = match candidates.as_slice() {
        [] => {
            let known: Vec<String> = before
                .iter()
                .map(|workspace| {
                    format!(
                        "group {} name {:?} at {}",
                        workspace.group,
                        workspace.name.as_deref().unwrap_or("?"),
                        coordinates_key(&workspace.coordinates)
                    )
                })
                .collect();
            bail!(
                "no workspace matches {target:?}{}; known workspaces: {}",
                group
                    .map(|index| format!(" in group {index}"))
                    .unwrap_or_default(),
                known.join("; ")
            );
        }
        [only] => only.handle.clone().context("workspace has no handle")?,
        many => {
            let groups: Vec<String> = many.iter().map(|w| w.group.to_string()).collect();
            bail!(
                "{target:?} exists in groups {}; pass \"group\" to say which output to switch",
                groups.join(", ")
            );
        }
    };

    handle.activate();
    // Workspace requests are double-buffered: nothing happens until commit.
    manager.commit();
    for _ in 0..4 {
        queue
            .roundtrip(&mut state)
            .context("roundtrip after activate")?;
    }
    manager.stop();
    Ok(collect(state))
}

fn coordinates_key(coordinates: &[u32]) -> String {
    coordinates
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

impl State {
    /// `collect` consumes its input, but activation needs the listing twice
    /// (to match, then to report the result) and the handles are cheap clones.
    fn clone_shallow(&self) -> State {
        State {
            workspaces: self.workspaces.clone(),
            groups: self.groups.clone(),
            members: self.members.clone(),
            done: self.done,
            finished: self.finished,
        }
    }
}

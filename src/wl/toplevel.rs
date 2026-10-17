//! Window enumeration over `ext_foreign_toplevel_list_v1`.
//!
//! This is the standard protocol (wayland-protocols staging), so `lswt` and
//! any other ext-foreign-toplevel client sees the same list. What it does not
//! carry is window state (activated / minimized / geometry) — that only exists
//! in `wlr-foreign-toplevel-management`, which COSMIC does not implement.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, event_created_child,
    backend::ObjectId,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Window {
    pub identifier: String,
    pub title: String,
    pub app_id: String,
}

#[derive(Default)]
struct State {
    entries: HashMap<ObjectId, Window>,
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

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                state.entries.entry(toplevel.id()).or_default();
            }
            ext_foreign_toplevel_list_v1::Event::Finished => state.finished = true,
            _ => {}
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
        let entry = state.entries.entry(proxy.id()).or_default();
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                entry.identifier = identifier;
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => entry.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => entry.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.entries.remove(&proxy.id());
            }
            _ => {}
        }
    }
}

pub fn list() -> Result<Vec<Window>> {
    let conn = Connection::connect_to_env().context("connect to WAYLAND_DISPLAY")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    let list: ExtForeignToplevelListV1 = globals
        .bind(&qh, 1..=1, ())
        .context("bind ext_foreign_toplevel_list_v1 (compositor does not support it)")?;

    let mut state = State::default();
    // The compositor announces existing toplevels right after the bind, but
    // title/app_id can trail in a later batch, so pump a few roundtrips.
    for _ in 0..4 {
        queue.roundtrip(&mut state).context("roundtrip for toplevels")?;
    }

    list.destroy();

    let mut windows: Vec<Window> = state
        .entries
        .into_values()
        .filter(|w| !w.identifier.is_empty())
        .collect();
    windows.sort_by(|a, b| a.app_id.cmp(&b.app_id).then(a.title.cmp(&b.title)));
    Ok(windows)
}

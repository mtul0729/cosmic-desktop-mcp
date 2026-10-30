//! MCP server over stdio, using the official Rust SDK.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig,
        Tool,
    },
    service::RequestContext,
    transport::io::stdio,
};
use serde_json::{Value, json};

use crate::applets;
use crate::clipboard;
use crate::config;
use crate::coords::{self, HitCandidate, HitResult, WindowGeometry};
use crate::input;
use crate::keymap::Button;
use crate::lockstate;
use crate::wl;
use crate::wl::toplevel::{self, Window};
use crate::wl::{capture, workspace};

pub fn serve() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create tokio runtime")?
        .block_on(async {
            let running = CosmicServer::default()
                .serve(stdio())
                .await
                .context("initialize MCP server")?;
            running.waiting().await.context("run MCP server")?;
            Ok(())
        })
}

#[derive(Clone, Default)]
struct CosmicServer {
    // The hand-written server handled one request at a time. Keep that
    // guarantee now that rmcp can dispatch requests concurrently: EIS and the
    // clipboard helpers are process-wide state, not independent operations.
    tool_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ServerHandler for CosmicServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("cosmic-desktop-mcp", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        let tools = tool_definitions().map_err(mcp_internal)?;
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        let _guard = self.tool_lock.lock().await;
        let name = request.name.into_owned();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let outcome = tokio::task::spawn_blocking(move || dispatch_tool(&name, &args))
            .await
            .map_err(|err| mcp_internal(anyhow::anyhow!("tool task failed: {err}")))?;

        Ok(tool_call_result(outcome).into())
    }
}

fn mcp_internal(err: anyhow::Error) -> McpError {
    McpError::internal_error(format!("{err:#}"), None)
}

fn tool_call_result(outcome: Result<String>) -> CallToolResult {
    match outcome {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(err) => CallToolResult::error(vec![ContentBlock::text(format!("{err:#}"))]),
    }
}

fn tool_definitions() -> Result<Vec<Tool>> {
    serde_json::from_value(json!([
        {
            "name": "screenshot",
            "description": "Capture one output and save it as a PNG. Returns the path, the image \
    size and \"screen_locked\" (null when logind could not be asked). This image is the pixel space \
    click/move/scroll accept for this output, with no downscaling applied. screen_locked=true means \
    the frame is the lock screen or a blanked desktop, not the user's session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "output": {
                        "type": "integer",
                        "description": "Output index (0 = primary). Default 0.",
                        "minimum": 0
                    }
                }
            }
        },
        {
            "name": "env_info",
            "description": "Report the current desktop topology and whether the input backend is \
    ready. Cheap, side-effect free: it never opens an EIS session, so it cannot raise a portal \
    authorization dialog. Call it before the first click when you do not know how many outputs \
    exist, or when a click seems to do nothing and you need to know whether input is authorized.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "list_windows",
            "description": "List open windows with identifier, title, app_id, COSMIC state \
    (maximized/minimized/activated/fullscreen/sticky), **placement** and **workspace membership**. \
    Geometry comes from zcosmic_toplevel_info_v1 and is reported twice: \"logical\" (compositor \
    coordinates relative to the output) and \"pixels\" (the same rect in that output's screenshot \
    space, which is what click/move/scroll take, together with \"output.index\"). \"pixels\" is null \
    until the input session exists, because the logical-to-pixel factor is only trustworthy when it \
    comes from the EIS region. \"workspaces\" lists every workspace the window is on (group, name, \
    active); \"on_active_workspace\" says whether one of them is the one its output currently shows. \
    Both are null when the compositor did not expose membership. A window on an inactive workspace \
    still has geometry — its last placement — so clicking those pixels hits whatever is visible \
    instead.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "screenshot_window",
            "description": "Capture one window: finds the window's output, screenshots it and crops \
    to the window rect. Returns JSON with the PNG path, the crop size (\"image\"), the window's \
    \"logical\" rect, its \"pixels\" rect in the output's screenshot space, the crop \"origin\", the \
    \"output\" (index + name) and the real \"scale\". The returned image is the **window image \
    pixel** space: pass a point from it straight to click_window / move_window / scroll_window, \
    which resolve the output and origin themselves. Do not add origin to the coordinates and call \
    click — that is the mistake this API is shaped to prevent. The crop shows whatever is on screen \
    in that region: if the window is covered (fullscreen video, another window on top) activate it \
    with window_action first. May open the input session (and a portal dialog) on first use, because \
    the crop needs the output's scale factor.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "Window identifier, app_id, or exact/unique title substring."
                    }
                },
                "required": ["window"]
            }
        },
        {
            "name": "window_action",
            "description": "Ask the compositor to act on a window: activate, close, maximize, \
    unmaximize, minimize, unminimize, fullscreen, unfullscreen (zcosmic_toplevel_manager_v1). \
    **Use activate before typing**: an EIS click does not move keyboard focus on COSMIC, so click + \
    type_text lands in whatever was focused before unless the window is activated first. The \
    compositor may ignore a request; re-list to confirm the resulting state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "Window identifier, app_id, or exact/unique title substring."
                    },
                    "action": {
                        "type": "string",
                        "enum": ["activate", "close", "maximize", "unmaximize", "minimize", "unminimize", "fullscreen", "unfullscreen"]
                    }
                },
                "required": ["window", "action"]
            }
        },
        {
            "name": "click",
            "description": "Click at (x,y) in the screenshot pixel space of the chosen output. \
    The output argument defaults to 0 and is never inferred from x/y, so coordinates taken from \
    another output's screenshot click the wrong monitor. If the point came from screenshot_window, \
    use click_window instead. The reply names the output actually used and the window under the \
    point (null = no window reported there), and warns when the same (x,y) lands inside a window on \
    a different output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "middle"],
                        "description": "Default left."
                    },
                    "count": {
                        "type": "integer",
                        "enum": [1, 2],
                        "description": "1 = single click, 2 = double click. Default 1."
                    },
                    "output": {
                        "type": "integer",
                        "description": "Output index from screenshot (default 0). Coordinates are in that output's pixel space."
                    },
                },
                "required": ["x", "y"]
            }
        },
        {
            "name": "move",
            "description": "Move the pointer without clicking (hover), in the screenshot pixel \
    space of the chosen output (default 0, never inferred from x/y). For a point taken from \
    screenshot_window use move_window. The reply reports the output used and the window under the \
    point.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "output": {
                        "type": "integer",
                        "description": "Output index from screenshot (default 0). Coordinates are in that output's pixel space."
                    },
                },
                "required": ["x", "y"]
            }
        },
        {
            "name": "scroll",
            "description": "Scroll the wheel at (x,y) in the screenshot pixel space of the chosen \
    output (default 0, never inferred from x/y). Positive dy scrolls UP (reveals earlier content, \
    matching the kernel REL_WHEEL convention), negative dy scrolls down; positive dx scrolls right. \
    For a point taken from screenshot_window use scroll_window. The reply reports the output used \
    and the window under the point.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "dy": {"type": "integer", "description": "Wheel clicks, vertical."},
                    "dx": {"type": "integer", "description": "Wheel clicks, horizontal."},
                    "output": {
                        "type": "integer",
                        "description": "Output index from screenshot (default 0). Coordinates are in that output's pixel space."
                    },
                },
                "required": ["x", "y"]
            }
        },
        {
            "name": "click_window",
            "description": "Click at (x,y) in the image returned by screenshot_window for the \
    named window. The tool resolves the window's output and the crop origin itself: there is no \
    output index and no arithmetic to get wrong. Use this instead of click for anything inside a \
    window, because click defaults to output 0 and cannot tell which monitor the coordinates came \
    from. The reply includes the image size, the output pixel and logical point actually used, and \
    the window under the point.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "Window identifier, app_id, or exact/unique title substring."
                    },
                    "x": {
                        "type": "integer",
                        "description": "Pixel x in the image returned by screenshot_window."
                    },
                    "y": {
                        "type": "integer",
                        "description": "Pixel y in the image returned by screenshot_window."
                    },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "middle"],
                        "description": "Default left."
                    },
                    "count": {
                        "type": "integer",
                        "enum": [1, 2],
                        "description": "1 = single click, 2 = double click. Default 1."
                    }
                },
                "required": ["window", "x", "y"]
            }
        },
        {
            "name": "move_window",
            "description": "Move the pointer to (x,y) in the image returned by screenshot_window \
    for the named window. Resolves the output and crop origin internally; use it instead of move for \
    a hover inside a window.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "Window identifier, app_id, or exact/unique title substring."
                    },
                    "x": {"type": "integer", "description": "Pixel x in the screenshot_window image."},
                    "y": {"type": "integer", "description": "Pixel y in the screenshot_window image."}
                },
                "required": ["window", "x", "y"]
            }
        },
        {
            "name": "scroll_window",
            "description": "Scroll the wheel at (x,y) in the image returned by screenshot_window \
    for the named window. Positive dy scrolls UP, negative down; positive dx scrolls right. Resolves \
    the output and crop origin internally.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "Window identifier, app_id, or exact/unique title substring."
                    },
                    "x": {"type": "integer", "description": "Pixel x in the screenshot_window image."},
                    "y": {"type": "integer", "description": "Pixel y in the screenshot_window image."},
                    "dy": {"type": "integer", "description": "Wheel clicks, vertical."},
                    "dx": {"type": "integer", "description": "Wheel clicks, horizontal."}
                },
                "required": ["window", "x", "y"]
            }
        },
        {
            "name": "type_text",
            "description": "Type text into the focused window. Default mode \"paste\" puts the text \
    on the clipboard and presses ctrl+v: this is the reliable path, because synthesized keycodes are \
    routed through the active input method and ASCII turns into pinyin candidates under fcitx5. Mode \
    \"keys\" injects keycodes instead (printable ASCII only) and is only correct when no input method \
    is active or real key events are wanted. Note that paste mode overwrites the clipboard.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "mode": {
                        "type": "string",
                        "enum": ["paste", "keys"],
                        "description": "paste (default) uses the clipboard + ctrl+v; keys sends keycodes."
                    }
                },
                "required": ["text"]
            }
        },
        {
            "name": "clipboard_write",
            "description": "Put text on the clipboard without pasting it. Useful when the target \
    cannot take ctrl+v, or when the same text is pasted several times. The entry lives as long as \
    this server process.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }
        },
        {
            "name": "clipboard_read",
            "description": "Read the current clipboard as text.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "key",
            "description": "Press a key combination, e.g. \"ctrl+s\", \"alt+tab\", \"Return\".",
            "inputSchema": {
                "type": "object",
                "properties": {"combo": {"type": "string"}},
                "required": ["combo"]
            }
        },
        {
            "name": "workspaces",
            "description": "List COSMIC workspaces over ext_workspace_manager_v1: group (one per \
    output), name, grid position, and active / urgent / hidden state. COSMIC workspaces are per \
    output, so every group has its own \"1\", \"2\", ... Pass \"activate\" to switch, plus \"group\" \
    when the name exists on several outputs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "activate": {
                        "type": "string",
                        "description": "Workspace name (\"2\") or grid position (\"2,0\") to switch to. Omit to just list."
                    },
                    "group": {
                        "type": "integer",
                        "description": "Which output's workspace group (0-based, from the listing). Required when the name exists in several groups.",
                        "minimum": 0
                    }
                }
            }
        },
        {
            "name": "applets",
            "description": "List running COSMIC panel applets as processes: name, pid, parent, \
    memory and age. Every applet is a normal process supervised by cosmic-panel, so there is one \
    instance per panel (two monitors = two copies of each applet).",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "restart_applet",
            "description": "Restart a panel applet by blanking and rewriting the cosmic-panel \
    plugins_* config entry that lists it. That is the only thing observed to make the panel respawn an \
    applet: a plain SIGTERM leaves the slot empty, because cosmic-panel does not supervise its applets \
    even though it builds a launch-pad ProcessManager. In practice the panel rebuilds every applet on \
    that section (time, workspaces and audio all get new pids), so treat this as a panel refresh. The \
    original config text is always written back, and a backup lands in \
    ~/.local/state/cosmic-desktop-mcp/panel-backups/. Accepts a full name (cosmic-applet-time) or the \
    suffix (time).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Full applet name or suffix, e.g. \"cosmic-applet-time\" or \"time\"."
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "app_logs",
            "description": "Recent stderr from one applet, straight out of the user journal \
    (journalctl -t <name>). Applets inherit the session's stderr, so this is where their warnings and \
    panics appear — the first place to look when a panel item misbehaves.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Applet process name, e.g. cosmic-applet-time."},
                    "lines": {"type": "integer", "description": "How many lines to return. Default 50, max 2000."}
                },
                "required": ["name"]
            }
        },
        {
            "name": "config_read",
            "description": "Read cosmic-config entries. COSMIC stores one setting per file as raw \
    RON text under ~/.config/cosmic/<app-id>/v1/<key>. Omit \"app\" to list app ids; omit \"key\" to \
    dump every key of one app.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "app": {"type": "string", "description": "App id, e.g. com.system76.CosmicAppletTime."},
                    "key": {"type": "string", "description": "Setting name, e.g. military_time."}
                }
            }
        },
        {
            "name": "config_write",
            "description": "Write or delete one cosmic-config key. The value is RON written \
    verbatim, so strings keep their quotes and enums keep their variant names. Returns the previous \
    value, which makes the change reversible. COSMIC watches the config tree and most consumers react \
    immediately; applets that only read at startup need restart_applet afterwards.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "app": {"type": "string"},
                    "key": {"type": "string"},
                    "value": {
                        "type": "string",
                        "description": "RON text, e.g. true, 0 or \"Some(\\\"text\\\")\". Omit or null to delete the key."
                    }
                },
                "required": ["app", "key"]
            }
        }
    ]))
    .context("built-in tool definitions do not match rmcp::model::Tool")
}

fn dispatch_tool(name: &str, args: &Value) -> Result<String> {
    match name {
        "screenshot" => tool_screenshot(args),
        "env_info" => tool_env_info(),
        "list_windows" => tool_list_windows(),
        "screenshot_window" => tool_screenshot_window(args),
        "window_action" => tool_window_action(args),
        "click" => tool_click(args),
        "move" => tool_move(args),
        "scroll" => tool_scroll(args),
        "click_window" => tool_click_window(args),
        "move_window" => tool_move_window(args),
        "scroll_window" => tool_scroll_window(args),
        "type_text" => tool_type_text(args),
        "clipboard_write" => tool_clipboard_write(args),
        "clipboard_read" => tool_clipboard_read(),
        "key" => tool_key(args),
        "workspaces" => tool_workspaces(args),
        "applets" => tool_applets(),
        "restart_applet" => tool_restart_applet(args),
        "app_logs" => tool_app_logs(args),
        "config_read" => tool_config_read(args),
        "config_write" => tool_config_write(args),
        other => bail!("unknown tool {other:?}"),
    }
}

fn tool_screenshot(args: &Value) -> Result<String> {
    let index = args.get("output").and_then(Value::as_u64).unwrap_or(0) as usize;
    let lock = lockstate::detect();
    let shot = capture::capture(index).with_context(|| {
        format!(
            "capturing output {index} failed; session lock state: {}",
            lock.evidence
        )
    })?;

    let dir = runtime_dir().join("cosmic-desktop-mcp");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("shot-{stamp}.png"));

    let image = image::RgbaImage::from_raw(shot.width, shot.height, shot.rgba)
        .context("captured frame did not match its declared size")?;
    image
        .save_with_format(&path, image::ImageFormat::Png)
        .with_context(|| format!("write {}", path.display()))?;

    let mut notes = vec![
        "this image is output screenshot pixel space: pass (x, y) from it to click/move/scroll \
         with the same output index"
            .to_string(),
    ];
    if lock.locked() == Some(true) {
        notes.push(
            "the session is locked: this frame is the lock screen or a blanked desktop, not the \
             user's desktop. Ask the user to unlock before acting on what you see."
                .to_string(),
        );
    }

    Ok(serde_json::to_string_pretty(&json!({
        "path": path.display().to_string(),
        "image": {"width": shot.width, "height": shot.height},
        "output": {
            "index": shot.output.index,
            "name": shot.output.name,
            "size": {"width": shot.output.width, "height": shot.output.height},
        },
        "screen_locked": lock.locked(),
        "lock_evidence": lock.evidence,
        "notes": notes,
        "summary": format!(
            "{}\n{}x{} output_index={} (coordinates are in this image's pixel space)",
            path.display(),
            shot.width,
            shot.height,
            shot.output.index
        ),
    }))?)
}

/// Current topology plus input readiness.
///
/// Uses `eis::peek`, never `eis::session`: opening a session can raise a portal
/// authorization dialog, and a status query must not do that. Consequently
/// `logical` is null until the first input tool call has authorized.
fn tool_env_info() -> Result<String> {
    let outputs = input::outputs()?;
    let session = crate::eis::peek();
    let regions = session
        .as_ref()
        .map(|session| session.regions())
        .unwrap_or_default();

    let mut rows = Vec::new();
    let mut problems = Vec::new();
    for out in &outputs {
        let (logical, scale, matched_by) = match input::region_for(out, &regions) {
            Ok(matched) => (
                json!({
                    "x": matched.region.x,
                    "y": matched.region.y,
                    "width": matched.region.width,
                    "height": matched.region.height,
                }),
                json!(matched.region.scale),
                json!(matched.by),
            ),
            Err(err) => {
                if session.is_some() {
                    problems.push(format!("output #{}: {err:#}", out.index));
                }
                (Value::Null, Value::Null, Value::Null)
            }
        };
        rows.push(json!({
            "index": out.index,
            "name": out.name,
            "physical": {"width": out.width, "height": out.height},
            "logical": logical,
            "scale": scale,
            "matched_by": matched_by,
        }));
    }

    let mut notes = vec![
        "click/move/scroll take pixel coordinates of a screenshot of the chosen output and \
         require that output index; the conversion to logical desktop coordinates happens \
         inside the tool."
            .to_string(),
        "for a point inside a window, use click_window/move_window/scroll_window with a point \
         from screenshot_window: those resolve the window's output and crop origin themselves, \
         so there is no output index to get wrong."
            .to_string(),
    ];
    if session.is_none() {
        notes.push(
            "the EIS session is not open yet: the first click opens one and may put a portal \
             authorization dialog on screen, which a human has to accept"
                .to_string(),
        );
    }
    notes.extend(problems);

    Ok(serde_json::to_string_pretty(&json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": ProtocolVersion::LATEST.as_str(),
        "protocol_versions": ProtocolVersion::KNOWN_VERSIONS,
        "input": {
            "backend": "eis",
            "connected": session.is_some(),
            "detail": session.as_ref().map(|session| session.summary_lines()),
        },
        "outputs": rows,
        "notes": notes,
    }))?)
}

fn tool_workspaces(args: &Value) -> Result<String> {
    let list = match args.get("activate").and_then(Value::as_str) {
        Some(target) => {
            let group = args
                .get("group")
                .and_then(Value::as_u64)
                .map(|index| index as usize);
            workspace::activate(target, group)?
        }
        None => workspace::list()?,
    };
    let groups = list
        .iter()
        .map(|workspace| workspace.group)
        .max()
        .map(|max| max + 1)
        .unwrap_or(0);
    Ok(serde_json::to_string_pretty(&json!({
        "count": list.len(),
        "groups": groups,
        "workspaces": list,
        "note": "COSMIC does not send the protocol's mandatory `id` event, so a workspace is \
                 identified by its group (one per output), name and grid position. Workspaces \
                 are per output: each group has its own \"1\", \"2\", ...",
    }))?)
}

fn tool_applets() -> Result<String> {
    let applets = applets::list()?;
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for applet in &applets {
        *counts.entry(applet.name.as_str()).or_default() += 1;
    }
    Ok(serde_json::to_string_pretty(&json!({
        "count": applets.len(),
        "by_name": counts,
        "applets": applets,
    }))?)
}

fn tool_restart_applet(args: &Value) -> Result<String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .context("restart_applet needs \"name\"")?;
    let restarted = applets::restart(name)?;
    Ok(serde_json::to_string_pretty(&json!({
        "restarted": restarted,
        "note": "the panel respawned these from its plugins_* config, so the pids differ from the \
                 processes that existed before; the config key was restored to its original text",
    }))?)
}

fn tool_app_logs(args: &Value) -> Result<String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .context("app_logs needs \"name\"")?;
    let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(50) as usize;
    applets::logs(name, lines)
}

fn tool_config_read(args: &Value) -> Result<String> {
    let app = args.get("app").and_then(Value::as_str);
    let key = args.get("key").and_then(Value::as_str);
    Ok(serde_json::to_string_pretty(&config::read(app, key)?)?)
}

fn tool_config_write(args: &Value) -> Result<String> {
    let app = args
        .get("app")
        .and_then(Value::as_str)
        .context("config_write needs \"app\"")?;
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .context("config_write needs \"key\"")?;
    // A missing or null value means "delete this key".
    let value = args.get("value").and_then(Value::as_str);
    Ok(serde_json::to_string_pretty(&config::write(
        app, key, value,
    )?)?)
}

/// Returns the index of the output with connector `name`, if still present.
fn output_index_for(name: &str, outputs: &[capture::OutputInfo]) -> Option<usize> {
    outputs
        .iter()
        .find(|out| out.name.as_deref() == Some(name))
        .map(|out| out.index)
}

/// Everything a pointer tool needs at one instant: windows, outputs, EIS
/// regions, and each window's geometry in all three coordinate spaces.
///
/// Built once per call so the hit test, the conversion and the reply all
/// describe the same desktop. `open_session = false` is for status-like calls:
/// peeking never raises a portal dialog, and without regions the pixel rects
/// are null rather than guessed.
struct Scene {
    windows: Vec<Window>,
    outputs: Vec<capture::OutputInfo>,
    regions: Vec<crate::eis::Region>,
    geoms: Vec<Option<WindowGeometry>>,
}

impl Scene {
    fn load(open_session: bool) -> Result<Self> {
        let windows = toplevel::list()?;
        Self::from_windows(windows, open_session)
    }

    fn from_windows(windows: Vec<Window>, open_session: bool) -> Result<Self> {
        let outputs = input::outputs()?;
        let regions = if open_session {
            crate::eis::session()?.regions()
        } else {
            crate::eis::peek()
                .map(|session| session.regions())
                .unwrap_or_default()
        };
        let geoms = windows
            .iter()
            .map(|window| {
                let index = window
                    .output
                    .as_deref()
                    .and_then(|name| output_index_for(name, &outputs))?;
                let out = outputs.get(index)?;
                let region = input::region_for(out, &regions).ok()?.region;
                WindowGeometry::new(
                    region,
                    index,
                    window.output.clone(),
                    window.logical?,
                    (out.width, out.height),
                )
            })
            .collect();
        Ok(Self {
            windows,
            outputs,
            regions,
            geoms,
        })
    }

    fn find_index(&self, needle: &str) -> Result<usize> {
        let target = toplevel::find(&self.windows, needle)?;
        self.windows
            .iter()
            .position(|window| window.identifier == target.identifier)
            .with_context(|| format!("window {:?} disappeared", target.title))
    }

    fn geometry(&self, index: usize) -> Result<&WindowGeometry> {
        self.geoms[index].as_ref().with_context(|| {
            format!(
                "{:?} has no usable geometry: the compositor may not have reported it yet, or \
                 the window lies entirely outside its output. Re-run list_windows.",
                self.windows[index].title
            )
        })
    }

    fn output(&self, index: usize) -> Result<&capture::OutputInfo> {
        self.outputs
            .get(index)
            .with_context(|| format!("no output #{index} (there are {})", self.outputs.len()))
    }

    /// Windows whose reported rect covers the point, excluding minimized ones
    /// and those on an inactive workspace: their geometry is a remembered
    /// placement, not what the output shows. Stacking order is not exposed, so
    /// the result may be ambiguous.
    fn hit(&self, output: usize, x: i32, y: i32) -> HitResult {
        let candidates: Vec<HitCandidate> = self
            .windows
            .iter()
            .enumerate()
            .filter(|(index, window)| {
                self.geoms[*index]
                    .as_ref()
                    .is_some_and(|geometry| geometry.output_index == output)
                    && window.on_active_workspace != Some(false)
                    && !window.state.iter().any(|state| state == "minimized")
            })
            .filter_map(|(index, window)| {
                let geometry = self.geoms[index].as_ref()?;
                Some(HitCandidate {
                    index,
                    rect: geometry.pixels,
                    activated: window.state.iter().any(|state| state == "activated"),
                })
            })
            .collect();
        coords::hit_test(&candidates, x, y)
    }
}

fn output_json(out: &capture::OutputInfo) -> Value {
    json!({
        "index": out.index,
        "name": out.name,
        "size": {"width": out.width, "height": out.height},
    })
}

fn window_ref(window: &Window) -> Value {
    json!({
        "identifier": window.identifier,
        "title": window.title,
        "app_id": window.app_id,
    })
}

fn hit_json(scene: &Scene, hit: &HitResult) -> Value {
    json!({
        "window": hit.window.map(|index| window_ref(&scene.windows[index])),
        "candidates": hit
            .candidates
            .iter()
            .map(|index| window_ref(&scene.windows[*index]))
            .collect::<Vec<_>>(),
        "ambiguous": hit.ambiguous,
        "basis": "window geometry as last reported, restricted to the active workspace and \
                  excluding minimized windows. The compositor exposes no stacking order, so \
                  several candidates mean window is the activated one or null when that is \
                  also ambiguous; null with no candidates means the desktop, another \
                  workspace, or a window that has not reported geometry.",
    })
}

/// The exact signature of the output-index mistake: the point is empty on the
/// requested output but lands inside a window on another one. Only consulted
/// when the requested output has no window under the point, so overlapping
/// coordinate ranges on two monitors do not produce noise.
fn other_output_match(scene: &Scene, requested: usize, x: i32, y: i32) -> Option<Value> {
    for (index, out) in scene.outputs.iter().enumerate() {
        if index == requested || x < 0 || y < 0 || x >= out.width || y >= out.height {
            continue;
        }
        let hit = scene.hit(index, x, y);
        let Some(found) = hit.window.or_else(|| hit.candidates.first().copied()) else {
            continue;
        };
        let window = &scene.windows[found];
        return Some(json!({
            "output": output_json(out),
            "window": window_ref(window),
            "warning": format!(
                "({x}, {y}) is not over a window on output {requested}, but the same point is \
                 inside {:?} on output {index}: these look like output {index}'s screenshot \
                 coordinates. Pass output={index}, or use click_window for a window image.",
                window.title
            ),
        }));
    }
    None
}

fn pointer_notes(window: &Window, geometry: &WindowGeometry) -> Vec<String> {
    let mut notes = Vec::new();
    if window.on_active_workspace == Some(false) {
        let where_ = window
            .workspaces
            .as_ref()
            .map(|workspaces| {
                workspaces
                    .iter()
                    .map(|workspace| {
                        format!(
                            "group {} {:?}",
                            workspace.group,
                            workspace.name.as_deref().unwrap_or("?")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "an inactive workspace".to_string());
        notes.push(format!(
            "the window is on {where_}, which is not active. Its geometry is the last reported \
             placement, so this action lands on whatever that output currently shows."
        ));
    }
    if window.state.iter().any(|state| state == "minimized") {
        notes.push(
            "the window is minimized: its geometry is the last placement, not visible pixels."
                .to_string(),
        );
    }
    if geometry.image.width != geometry.pixels.width
        || geometry.image.height != geometry.pixels.height
    {
        notes.push(
            "the window is partly off-screen; only the visible part is in the image.".to_string(),
        );
    }
    notes
}

fn lock_notes() -> (Option<bool>, String, Vec<String>) {
    let report = lockstate::detect();
    let mut notes = Vec::new();
    if report.locked() == Some(true) {
        notes.push(
            "the session is locked: this input goes to the lock screen, not the user's desktop."
                .to_string(),
        );
    }
    (report.locked(), report.evidence, notes)
}

fn logical_json(x: f64, y: f64) -> Value {
    let round = |value: f64| (value * 1000.0).round() / 1000.0;
    json!({"x": round(x), "y": round(y)})
}

fn point_in_output(out: &capture::OutputInfo, x: i32, y: i32) -> Result<()> {
    if x < 0 || y < 0 || x >= out.width || y >= out.height {
        bail!(
            "point ({x}, {y}) is outside output #{} {}, whose screenshot is {}x{}. Coordinates \
             belong to one output: use the output the screenshot came from, or click_window \
             for a point inside a screenshot_window image.",
            out.index,
            out.name.as_deref().unwrap_or("<unnamed>"),
            out.width,
            out.height
        );
    }
    Ok(())
}

fn tool_list_windows() -> Result<String> {
    let scene = Scene::load(false)?;
    let (locked, lock_evidence, _) = lock_notes();

    let rows: Vec<Value> = scene
        .windows
        .iter()
        .enumerate()
        .map(|(position, window)| {
            let geometry = scene.geoms[position].as_ref();
            let index = window
                .output
                .as_deref()
                .and_then(|name| output_index_for(name, &scene.outputs));
            let output = index.map(|index| {
                let out = &scene.outputs[index];
                json!({
                    "index": index,
                    "name": window.output,
                    "size": {"width": out.width, "height": out.height},
                    "scale": geometry.map(|geometry| geometry.region.scale),
                })
            });
            json!({
                "identifier": window.identifier,
                "title": window.title,
                "app_id": window.app_id,
                "state": window.state,
                "output": output,
                "logical": window.logical,
                "pixels": geometry.map(|geometry| geometry.pixels),
                "workspaces": window.workspaces,
                "on_active_workspace": window.on_active_workspace,
            })
        })
        .collect();

    let mut notes = vec![
        "`logical` is the compositor's placement relative to `output`, in logical coordinates; \
         `pixels` is the same rect in that output's screenshot pixel space, which is what \
         click/move/scroll accept (with the same output.index). `pixels` is null until the EIS \
         session exists."
            .to_string(),
        "for a point inside a window, take it from screenshot_window and call click_window: it \
         resolves the output and the crop origin itself, so there is no output index to get \
         wrong and no arithmetic."
            .to_string(),
        "geometry is placement as last reported, not visibility: a fullscreen or simply \
         overlapping window still sits at its tiled coordinates, so the region can show \
         something else. on_active_workspace=false is the stronger case — the window is on a \
         workspace that output does not currently show."
            .to_string(),
    ];
    if scene.windows.iter().any(|window| window.logical.is_none()) {
        notes.push(
            "some windows have no geometry yet: the compositor sends it on the first map and \
             again on every move/resize, so re-list after a moment"
                .to_string(),
        );
    }
    if scene
        .windows
        .iter()
        .any(|window| window.workspaces.is_none())
    {
        notes.push(
            "workspaces is null for some windows: this compositor did not report workspace \
             membership (zcosmic_toplevel_info_v1 v3 ext_workspace_enter plus \
             ext_workspace_manager_v1). That is unknown, not 'on no workspace'."
                .to_string(),
        );
    }

    Ok(serde_json::to_string_pretty(&json!({
        "count": rows.len(),
        "windows": rows,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "notes": notes,
    }))?)
}

/// Crops the window out of a screenshot of its output and reports the same
/// geometry fields as `list_windows`.
///
/// Needs the EIS region for the scale factor, so the first call may open a
/// session (and with it a portal authorization dialog).
fn tool_screenshot_window(args: &Value) -> Result<String> {
    let needle = args
        .get("window")
        .and_then(Value::as_str)
        .context("screenshot_window needs \"window\" (identifier, app_id or title)")?;
    // Resolve the window before opening a session, so a typo does not raise a
    // portal dialog.
    let windows = toplevel::list()?;
    let target = toplevel::find(&windows, needle)?;
    let scene = Scene::from_windows(windows, true)?;
    let index = scene.find_index(&target.identifier)?;
    let base = scene.geometry(index)?.clone();
    let window = &scene.windows[index];
    let out = scene.output(base.output_index)?;

    let shot = capture::capture(base.output_index)?;
    // Clamp against the frame's own size: it should equal the wl_output mode,
    // but the frame is what the pixels actually come from.
    let geometry = WindowGeometry::new(
        base.region,
        base.output_index,
        base.output_name,
        base.logical,
        (shot.width as i32, shot.height as i32),
    )
    .with_context(|| {
        format!(
            "{:?} lies entirely outside output #{} ({}x{})",
            target.title, out.index, shot.width, shot.height
        )
    })?;

    let image = image::RgbaImage::from_raw(shot.width, shot.height, shot.rgba)
        .context("captured frame did not match its declared size")?;
    let cropped = image::imageops::crop_imm(
        &image,
        geometry.image.x as u32,
        geometry.image.y as u32,
        geometry.image.width as u32,
        geometry.image.height as u32,
    )
    .to_image();

    let dir = runtime_dir().join("cosmic-desktop-mcp");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("window-{stamp}.png"));
    cropped
        .save_with_format(&path, image::ImageFormat::Png)
        .with_context(|| format!("write {}", path.display()))?;

    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.extend(pointer_notes(window, &geometry));
    notes.push(
        "the returned image is window image pixel space: pass (x, y) from it to click_window / \
         move_window / scroll_window for this window. Do not add the origin and call click."
            .to_string(),
    );
    let name = geometry
        .output_name
        .clone()
        .unwrap_or_else(|| "?".to_string());
    let width = geometry.image.width;
    let height = geometry.image.height;

    Ok(serde_json::to_string_pretty(&json!({
        "path": path.display().to_string(),
        "image": {"width": width, "height": height},
        "window": window_ref(window),
        "output": {
            "index": geometry.output_index,
            "name": geometry.output_name,
            "size": {"width": shot.width, "height": shot.height},
            "scale": geometry.region.scale,
        },
        "scale": geometry.region.scale,
        "logical": geometry.logical,
        "pixels": geometry.pixels,
        "origin": {"x": geometry.image.x, "y": geometry.image.y},
        "workspaces": window.workspaces,
        "on_active_workspace": window.on_active_workspace,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "notes": notes,
        "summary": format!(
            "{}\n{}x{} from output_index={} ({}), window {:?}\nwindow origin in that output's \
             screenshot pixels: x={} y={}\nfor a point (dx,dy) inside this image use \
             click_window(window={:?}, x=dx, y=dy) — no output index and no arithmetic",
            path.display(),
            width,
            height,
            geometry.output_index,
            name,
            window.title,
            geometry.image.x,
            geometry.image.y,
            window.title
        ),
    }))?)
}

fn tool_window_action(args: &Value) -> Result<String> {
    let needle = args
        .get("window")
        .and_then(Value::as_str)
        .context("window_action needs \"window\" (identifier, app_id or title)")?;
    let action = toplevel::Action::parse(
        args.get("action")
            .and_then(Value::as_str)
            .context("window_action needs \"action\"")?,
    )?;
    let done = toplevel::act(needle, action)?;
    Ok(format!(
        "{done}\nthe compositor may ignore a request; re-run list_windows to see the resulting state"
    ))
}

fn button_arg(args: &Value) -> Result<Button> {
    match args.get("button").and_then(Value::as_str) {
        Some(name) => Button::parse(name),
        None => Ok(Button::Left),
    }
}

fn count_arg(args: &Value) -> u32 {
    args.get("count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as u32
}

fn window_arg<'a>(args: &'a Value, tool: &str) -> Result<&'a str> {
    args.get("window")
        .and_then(Value::as_str)
        .with_context(|| format!("{tool} needs \"window\" (identifier, app_id or title)"))
}

/// Resolves a window-image point into every space, using the scene's geometry.
fn window_point(
    scene: &Scene,
    needle: &str,
    x: i32,
    y: i32,
) -> Result<(usize, WindowGeometry, i32, i32, f64, f64)> {
    let index = scene.find_index(needle)?;
    let geometry = scene.geometry(index)?.clone();
    let (ox, oy) = geometry.window_to_output(x, y)?;
    let (lx, ly) = geometry.window_to_logical(x, y)?;
    Ok((index, geometry, ox, oy, lx, ly))
}

fn tool_click(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let button = button_arg(args)?;
    let count = count_arg(args);
    let output = output_arg(args);

    let scene = Scene::load(true)?;
    let out = scene.output(output)?;
    point_in_output(out, x, y)?;
    let region = input::region_for(out, &scene.regions)?.region;
    let (lx, ly) = (
        region.x + f64::from(x) / region.scale,
        region.y + f64::from(y) / region.scale,
    );
    let hit = scene.hit(output, x, y);
    let other = if hit.candidates.is_empty() {
        other_output_match(&scene, output, x, y)
    } else {
        None
    };

    input::click_logical(button, lx, ly, count)?;
    let (locked, lock_evidence, notes) = lock_notes();

    Ok(serde_json::to_string_pretty(&json!({
        "summary": format!("clicked {button:?} at ({x}, {y}) on output {output} x{count}"),
        "action": "click",
        "button": format!("{button:?}"),
        "count": count,
        "requested": {"x": x, "y": y, "output": output},
        "output": output_json(out),
        "logical": logical_json(lx, ly),
        "hit": hit_json(&scene, &hit),
        "other_output_match": other,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "notes": notes,
    }))?)
}

fn tool_move(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let output = output_arg(args);

    let scene = Scene::load(true)?;
    let out = scene.output(output)?;
    point_in_output(out, x, y)?;
    let region = input::region_for(out, &scene.regions)?.region;
    let (lx, ly) = (
        region.x + f64::from(x) / region.scale,
        region.y + f64::from(y) / region.scale,
    );
    let hit = scene.hit(output, x, y);
    let other = if hit.candidates.is_empty() {
        other_output_match(&scene, output, x, y)
    } else {
        None
    };

    input::move_logical(lx, ly)?;
    let (locked, lock_evidence, notes) = lock_notes();

    Ok(serde_json::to_string_pretty(&json!({
        "summary": format!("pointer at ({x}, {y}) on output {output}"),
        "action": "move",
        "requested": {"x": x, "y": y, "output": output},
        "output": output_json(out),
        "logical": logical_json(lx, ly),
        "hit": hit_json(&scene, &hit),
        "other_output_match": other,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "notes": notes,
    }))?)
}

fn tool_scroll(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let dy = args.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32;
    let dx = args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;
    let output = output_arg(args);

    let scene = Scene::load(true)?;
    let out = scene.output(output)?;
    point_in_output(out, x, y)?;
    let region = input::region_for(out, &scene.regions)?.region;
    let (lx, ly) = (
        region.x + f64::from(x) / region.scale,
        region.y + f64::from(y) / region.scale,
    );
    let hit = scene.hit(output, x, y);
    let other = if hit.candidates.is_empty() {
        other_output_match(&scene, output, x, y)
    } else {
        None
    };

    input::scroll_logical(lx, ly, dx, dy)?;
    let (locked, lock_evidence, notes) = lock_notes();

    Ok(serde_json::to_string_pretty(&json!({
        "summary": format!("scrolled dx={dx} dy={dy} at ({x}, {y}) on output {output}"),
        "action": "scroll",
        "dx": dx,
        "dy": dy,
        "requested": {"x": x, "y": y, "output": output},
        "output": output_json(out),
        "logical": logical_json(lx, ly),
        "hit": hit_json(&scene, &hit),
        "other_output_match": other,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "notes": notes,
    }))?)
}

fn tool_click_window(args: &Value) -> Result<String> {
    let needle = window_arg(args, "click_window")?;
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let button = button_arg(args)?;
    let count = count_arg(args);

    let windows = toplevel::list()?;
    let target = toplevel::find(&windows, needle)?;
    let scene = Scene::from_windows(windows, true)?;
    let (index, geometry, ox, oy, lx, ly) = window_point(&scene, &target.identifier, x, y)?;
    let window = &scene.windows[index];
    let out = scene.output(geometry.output_index)?;
    let hit = scene.hit(geometry.output_index, ox, oy);
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.extend(pointer_notes(window, &geometry));

    input::click_logical(button, lx, ly, count)?;

    Ok(serde_json::to_string_pretty(&json!({
        "summary": format!(
            "clicked {button:?} in window {:?} at image ({x}, {y}) = output {} pixel ({ox}, {oy}) x{count}",
            window.title, geometry.output_index
        ),
        "action": "click",
        "button": format!("{button:?}"),
        "count": count,
        "window": window_ref(window),
        "output": output_json(out),
        "scale": geometry.region.scale,
        "image": {"width": geometry.image.width, "height": geometry.image.height},
        "point": {
            "image": {"x": x, "y": y},
            "output": {"x": ox, "y": oy},
            "logical": logical_json(lx, ly),
        },
        "hit": hit_json(&scene, &hit),
        "on_active_workspace": window.on_active_workspace,
        "workspaces": window.workspaces,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "notes": notes,
    }))?)
}

fn tool_move_window(args: &Value) -> Result<String> {
    let needle = window_arg(args, "move_window")?;
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;

    let windows = toplevel::list()?;
    let target = toplevel::find(&windows, needle)?;
    let scene = Scene::from_windows(windows, true)?;
    let (index, geometry, ox, oy, lx, ly) = window_point(&scene, &target.identifier, x, y)?;
    let window = &scene.windows[index];
    let out = scene.output(geometry.output_index)?;
    let hit = scene.hit(geometry.output_index, ox, oy);
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.extend(pointer_notes(window, &geometry));

    input::move_logical(lx, ly)?;

    Ok(serde_json::to_string_pretty(&json!({
        "summary": format!(
            "pointer at window {:?} image ({x}, {y}) = output {} pixel ({ox}, {oy})",
            window.title, geometry.output_index
        ),
        "action": "move",
        "window": window_ref(window),
        "output": output_json(out),
        "scale": geometry.region.scale,
        "image": {"width": geometry.image.width, "height": geometry.image.height},
        "point": {
            "image": {"x": x, "y": y},
            "output": {"x": ox, "y": oy},
            "logical": logical_json(lx, ly),
        },
        "hit": hit_json(&scene, &hit),
        "on_active_workspace": window.on_active_workspace,
        "workspaces": window.workspaces,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "notes": notes,
    }))?)
}

fn tool_scroll_window(args: &Value) -> Result<String> {
    let needle = window_arg(args, "scroll_window")?;
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let dy = args.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32;
    let dx = args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;

    let windows = toplevel::list()?;
    let target = toplevel::find(&windows, needle)?;
    let scene = Scene::from_windows(windows, true)?;
    let (index, geometry, ox, oy, lx, ly) = window_point(&scene, &target.identifier, x, y)?;
    let window = &scene.windows[index];
    let out = scene.output(geometry.output_index)?;
    let hit = scene.hit(geometry.output_index, ox, oy);
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.extend(pointer_notes(window, &geometry));

    input::scroll_logical(lx, ly, dx, dy)?;

    Ok(serde_json::to_string_pretty(&json!({
        "summary": format!(
            "scrolled dx={dx} dy={dy} in window {:?} at image ({x}, {y}) = output {} pixel ({ox}, {oy})",
            window.title, geometry.output_index
        ),
        "action": "scroll",
        "dx": dx,
        "dy": dy,
        "window": window_ref(window),
        "output": output_json(out),
        "scale": geometry.region.scale,
        "image": {"width": geometry.image.width, "height": geometry.image.height},
        "point": {
            "image": {"x": x, "y": y},
            "output": {"x": ox, "y": oy},
            "logical": logical_json(lx, ly),
        },
        "hit": hit_json(&scene, &hit),
        "on_active_workspace": window.on_active_workspace,
        "workspaces": window.workspaces,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "notes": notes,
    }))?)
}

fn tool_type_text(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .context("missing text")?;
    if text.is_empty() {
        return Ok("nothing to type".to_string());
    }

    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("paste");
    match mode {
        // Keycodes go through the compositor's input pipeline, which hands
        // them to whatever input method is active. With fcitx5 running, "abc"
        // is treated as a pinyin syllable and commits as a Chinese candidate:
        // "abc123" arrived as Chinese characters followed by "23". So keycodes
        // are opt-in, and only for callers that know the input method is off or
        // that the target wants real key events.
        "keys" => {
            let keyable = text
                .chars()
                .all(|c| c.is_ascii_graphic() || c == ' ' || c == '\n' || c == '\t');
            if !keyable {
                bail!("mode=keys only accepts printable ASCII; use mode=paste for anything else");
            }
            input::type_keys(text)?;
            Ok(format!(
                "typed {} characters as keycodes",
                text.chars().count()
            ))
        }
        "paste" => {
            clipboard::write(text)?;
            // The compositor needs to see the new selection before the paste
            // request, otherwise the target reads the previous contents.
            std::thread::sleep(Duration::from_millis(120));
            press_chord("ctrl+v")?;
            Ok(format!(
                "pasted {} characters via the clipboard (ctrl+v); the clipboard now holds this text",
                text.chars().count()
            ))
        }
        other => bail!("unknown mode {other:?}; expected \"paste\" or \"keys\""),
    }
}

fn tool_clipboard_write(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .context("missing text")?;
    clipboard::write(text)?;
    Ok(format!(
        "clipboard set ({} characters)",
        text.chars().count()
    ))
}

fn tool_clipboard_read() -> Result<String> {
    clipboard::read()
}

fn tool_key(args: &Value) -> Result<String> {
    let combo = args
        .get("combo")
        .and_then(Value::as_str)
        .context("missing combo")?;
    press_chord(combo)?;
    Ok(format!("pressed {combo}"))
}

fn press_chord(combo: &str) -> Result<()> {
    input::chord(combo)
}

fn output_arg(args: &Value) -> usize {
    args.get("output").and_then(Value::as_u64).unwrap_or(0) as usize
}

fn int_arg(args: &Value, key: &str) -> Result<i32> {
    let value = args
        .get(key)
        .and_then(Value::as_i64)
        .with_context(|| format!("missing integer argument {key:?}"))?;
    i32::try_from(value).with_context(|| format!("argument {key:?} out of range"))
}

fn runtime_dir() -> PathBuf {
    crate::session::runtime_dir().unwrap_or_else(|_| std::env::temp_dir())
}

/// `--doctor` output: proves the three moving parts work without running a
/// tool call against a live desktop.
pub fn doctor() -> Result<()> {
    match wl::socket_path() {
        Ok(socket) => println!("wayland socket: {}", socket.display()),
        Err(err) => println!("wayland socket: {err:#}"),
    }
    let outputs = capture::outputs().context("enumerate outputs")?;
    for output in &outputs {
        println!(
            "output #{}: {}x{} scale={} name={}",
            output.index,
            output.width,
            output.height,
            output.scale,
            output.name.as_deref().unwrap_or("?")
        );
    }
    let windows = toplevel::list().context("list windows")?;
    println!("toplevels: {}", windows.len());
    for window in &windows {
        println!(
            "  {} [{}] {}",
            window.app_id, window.identifier, window.title
        );
    }
    for index in 0..outputs.len() {
        match capture::capture(index) {
            Ok(shot) => println!(
                "capture #{}: {}x{} ok ({} bytes rgba)",
                index,
                shot.width,
                shot.height,
                shot.rgba.len()
            ),
            Err(err) => println!("capture #{index}: {err:#}"),
        }
    }
    print!("{}", input::describe().context("probe the input backend")?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rmcp_advertises_the_expected_server_and_tool_capability() {
        let info = ServerHandler::get_info(&CosmicServer::default());
        assert_eq!(info.server_info.name, "cosmic-desktop-mcp");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.capabilities.tools.is_some());
        assert!(ProtocolVersion::KNOWN_VERSIONS.contains(&ProtocolVersion::LATEST));
    }

    #[test]
    fn every_tool_definition_deserializes_as_an_rmcp_tool() {
        let tools = tool_definitions().expect("tool definitions must match rmcp::model::Tool");
        let names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                "screenshot",
                "env_info",
                "list_windows",
                "screenshot_window",
                "window_action",
                "click",
                "move",
                "scroll",
                "click_window",
                "move_window",
                "scroll_window",
                "type_text",
                "clipboard_write",
                "clipboard_read",
                "key",
                "workspaces",
                "applets",
                "restart_applet",
                "app_logs",
                "config_read",
                "config_write",
            ]
        );
        for tool in tools {
            assert_eq!(
                tool.input_schema.get("type").and_then(Value::as_str),
                Some("object"),
                "{} must have an object input schema",
                tool.name
            );
        }
    }

    #[test]
    fn rmcp_tool_results_keep_tool_errors_visible_to_the_client() {
        let success = tool_call_result(Ok("ok".to_string()));
        assert_eq!(success.is_error, Some(false));
        assert_eq!(
            success.content[0].as_text().map(|text| text.text.as_str()),
            Some("ok")
        );

        let failure = tool_call_result(Err(anyhow::anyhow!("tool failed: detail")));
        assert_eq!(failure.is_error, Some(true));
        assert_eq!(
            failure.content[0].as_text().map(|text| text.text.as_str()),
            Some("tool failed: detail")
        );
    }
}

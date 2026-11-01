//! MCP server over stdio, using the official Rust SDK.

use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ProtocolVersion,
        ServerCapabilities, ServerConfig, Tool,
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
use crate::wl::toplevel::{self, Rect, Window};
use crate::wl::{capture, workspace};

const INPUT_VERIFICATION_NOTE: &str = "EIS accepted the input request, but the compositor does not \
acknowledge delivery; take a new screenshot to verify the result.";

pub fn serve() -> Result<()> {
    let result = tokio::runtime::Builder::new_multi_thread()
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
        });
    clipboard::shutdown();
    result
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
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        let _guard = self.tool_lock.lock().await;
        let name = request.name.into_owned();
        let args = Value::Object(request.arguments.unwrap_or_default());

        let progress = if needs_input_authorization(&name) && crate::eis::peek().is_none() {
            context.meta.get_progress_token().map(|token| {
                let peer = context.peer.clone();
                tokio::spawn(async move {
                    let mut step = 0.0;
                    loop {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        step += 1.0;
                        let notification = ProgressNotificationParam::new(token.clone(), step)
                            .with_message(
                                "waiting for the system RemoteDesktop authorization dialog; ask \
                                 the human to approve it",
                            );
                        if peer.notify_progress(notification).await.is_err() {
                            break;
                        }
                    }
                })
            })
        } else {
            None
        };

        let outcome = tokio::task::spawn_blocking(move || dispatch_tool(&name, &args)).await;
        if let Some(progress) = progress {
            progress.abort();
        }
        let outcome =
            outcome.map_err(|err| mcp_internal(anyhow::anyhow!("tool task failed: {err}")))?;

        Ok(tool_call_result(outcome).into())
    }
}

fn mcp_internal(err: anyhow::Error) -> McpError {
    McpError::internal_error(format!("{err:#}"), None)
}

struct ToolResponse {
    text: String,
    structured: Option<Value>,
    is_error: bool,
}

impl ToolResponse {
    fn success(text: String) -> Self {
        let structured = serde_json::from_str(&text).ok();
        Self {
            text,
            structured,
            is_error: false,
        }
    }

    fn error(structured: Value) -> Self {
        Self {
            text: serde_json::to_string_pretty(&structured)
                .unwrap_or_else(|_| structured.to_string()),
            structured: Some(structured),
            is_error: true,
        }
    }
}

fn tool_call_result(outcome: Result<ToolResponse>) -> CallToolResult {
    let response = match outcome {
        Ok(response) => response,
        Err(err) => {
            let text = format!("{err:#}");
            let structured = classify_tool_error(&text);
            ToolResponse {
                text,
                structured,
                is_error: true,
            }
        }
    };
    let mut result = if response.is_error {
        CallToolResult::error(vec![ContentBlock::text(response.text)])
    } else {
        CallToolResult::success(vec![ContentBlock::text(response.text)])
    };
    result.structured_content = response.structured;
    result
}

fn classify_tool_error(text: &str) -> Option<Value> {
    let lower = text.to_ascii_lowercase();
    let status = if lower.contains("did not come up within") {
        "authorization_timeout"
    } else if lower.contains("cancel")
        || lower.contains("denied")
        || lower.contains("did not grant the session")
        || lower.contains("granted no input devices")
        || lower.contains("was rejected")
    {
        "authorization_denied"
    } else if lower.contains("remote desktop portal")
        || lower.contains("remotedesktop portal")
        || lower.contains("connect to eis")
        || lower.contains("portal session")
        || lower.contains("org.freedesktop.portal")
    {
        "authorization_unavailable"
    } else {
        return None;
    };
    let next_action = if status == "authorization_unavailable" {
        "check that the XDG Desktop Portal and its RemoteDesktop backend are running, then retry"
    } else {
        "ask the human to approve the system RemoteDesktop authorization dialog, then retry"
    };
    Some(json!({
        "status": status,
        "message": text,
        "next_action": next_action,
    }))
}

fn needs_input_authorization(tool: &str) -> bool {
    matches!(
        tool,
        "prepare_input"
            | "click"
            | "move"
            | "scroll"
            | "click_window"
            | "move_window"
            | "scroll_window"
            | "type_text"
            | "key"
    )
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
            "name": "prepare_input",
            "description": "Open or reuse the EIS input session and report its devices and output \
    regions. This is the only status-like tool with a possible side effect: on first use it may \
    show the system RemoteDesktop authorization dialog, and a human must approve it. Actual input \
    tools also open the session on first use, so calling this is optional; use it when you want to \
    trigger and diagnose authorization before taking an action.",
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
            "description": "Capture one window without opening the EIS input session. Prefers the \
    compositor's direct window capture source; if that is unavailable or not coordinate-safe, it \
    can use an output crop only when an input session is already open. Returns the PNG path, a \
    \"coordinates\" object, and capture metadata. Pass that coordinates object unchanged to \
    click_window / move_window / scroll_window. If the response is a needs_input_session error, \
    call prepare_input explicitly and take a new screenshot; this tool never opens the portal.",
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
            "description": "Click at (x,y) in the image returned by screenshot_window. Pass that \
    result's \"coordinates\" object unchanged: it identifies the screenshot's capture space, image \
    size and logical window rect. The tool validates freshness, opens/reuses EIS, and resolves the \
    output pixel and logical point itself. Use this instead of click for anything inside a window.",
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
                    "coordinates": {
                        "type": "object",
                        "description": "The coordinates object returned by screenshot_window, unchanged."
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
                "required": ["window", "x", "y", "coordinates"]
            }
        },
        {
            "name": "move_window",
            "description": "Move the pointer to (x,y) in the image returned by screenshot_window. \
    Pass that result's \"coordinates\" object unchanged; the tool validates freshness and resolves \
    the output and logical point itself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "Window identifier, app_id, or exact/unique title substring."
                    },
                    "x": {"type": "integer", "description": "Pixel x in the screenshot_window image."},
                    "y": {"type": "integer", "description": "Pixel y in the screenshot_window image."},
                    "coordinates": {
                        "type": "object",
                        "description": "The coordinates object returned by screenshot_window, unchanged."
                    }
                },
                "required": ["window", "x", "y", "coordinates"]
            }
        },
        {
            "name": "scroll_window",
            "description": "Scroll the wheel at (x,y) in the image returned by screenshot_window. \
    Positive dy scrolls UP, negative down; positive dx scrolls right. Pass that result's \
    \"coordinates\" object unchanged.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "Window identifier, app_id, or exact/unique title substring."
                    },
                    "x": {"type": "integer", "description": "Pixel x in the screenshot_window image."},
                    "y": {"type": "integer", "description": "Pixel y in the screenshot_window image."},
                    "coordinates": {
                        "type": "object",
                        "description": "The coordinates object returned by screenshot_window, unchanged."
                    },
                    "dy": {"type": "integer", "description": "Wheel clicks, vertical."},
                    "dx": {"type": "integer", "description": "Wheel clicks, horizontal."}
                },
                "required": ["window", "x", "y", "coordinates"]
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
    original config text is always written back, and a private backup lands under \
    $XDG_STATE_HOME/cosmic-desktop-mcp/panel-backups/ (or ~/.local/state/...). Accepts a full name \
    (cosmic-applet-time) or the suffix (time).",
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

fn dispatch_tool(name: &str, args: &Value) -> Result<ToolResponse> {
    let response = match name {
        "screenshot" => ToolResponse::success(tool_screenshot(args)?),
        "env_info" => ToolResponse::success(tool_env_info()?),
        "prepare_input" => ToolResponse::success(tool_prepare_input()?),
        "list_windows" => ToolResponse::success(tool_list_windows()?),
        "screenshot_window" => tool_screenshot_window(args)?,
        "window_action" => ToolResponse::success(tool_window_action(args)?),
        "click" => ToolResponse::success(tool_click(args)?),
        "move" => ToolResponse::success(tool_move(args)?),
        "scroll" => ToolResponse::success(tool_scroll(args)?),
        "click_window" => ToolResponse::success(tool_click_window(args)?),
        "move_window" => ToolResponse::success(tool_move_window(args)?),
        "scroll_window" => ToolResponse::success(tool_scroll_window(args)?),
        "type_text" => ToolResponse::success(tool_type_text(args)?),
        "key" => ToolResponse::success(tool_key(args)?),
        "workspaces" => ToolResponse::success(tool_workspaces(args)?),
        "applets" => ToolResponse::success(tool_applets()?),
        "restart_applet" => ToolResponse::success(tool_restart_applet(args)?),
        "config_read" => ToolResponse::success(tool_config_read(args)?),
        "config_write" => ToolResponse::success(tool_config_write(args)?),
        other => bail!("unknown tool {other:?}"),
    };
    Ok(response)
}

fn tool_screenshot(args: &Value) -> Result<String> {
    let index = output_arg(args)?;
    let lock = lockstate::detect();
    let shot = capture::capture(index).with_context(|| {
        format!(
            "capturing output {index} failed; session lock state: {}",
            lock.evidence
        )
    })?;

    let image = image::RgbaImage::from_raw(shot.width, shot.height, shot.rgba)
        .context("captured frame did not match its declared size")?;
    let path = save_png(&image, "shot")?;

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
            "the EIS session is not open yet. Observation tools do not open it. The first actual \
             input action, or an explicit prepare_input call, may put a portal authorization \
             dialog on screen; a human must accept it"
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

/// Explicitly opens the process-wide EIS session.
///
/// Unlike every observation tool, this may put a RemoteDesktop authorization
/// dialog on screen. Actual input tools keep the same lazy behaviour.
fn tool_prepare_input() -> Result<String> {
    let already_open = crate::eis::peek().is_some();
    let session = crate::eis::session()?;
    let regions: Vec<Value> = session
        .regions()
        .into_iter()
        .map(|region| {
            let (physical_width, physical_height) = region.physical_size();
            json!({
                "mapping_id": region.mapping_id,
                "logical": {
                    "x": region.x,
                    "y": region.y,
                    "width": region.width,
                    "height": region.height,
                },
                "physical": {"width": physical_width, "height": physical_height},
                "scale": region.scale,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&json!({
        "ready": true,
        "opened": !already_open,
        "side_effects": if already_open {
            Vec::<String>::new()
        } else {
            vec![
                "the RemoteDesktop portal may have shown an authorization dialog before this \
                 result"
                    .to_string(),
            ]
        },
        "regions": regions,
        "detail": session.summary_lines(),
    }))?)
}

fn tool_workspaces(args: &Value) -> Result<String> {
    let activate = optional_string_arg(args, "activate")?;
    let group = optional_u64_arg(args, "group")?
        .map(|index| usize::try_from(index).context("workspace group index is out of range"))
        .transpose()?;
    if activate.is_none() && group.is_some() {
        bail!("workspaces \"group\" requires \"activate\"");
    }
    let list = match activate {
        Some(target) => workspace::activate(target, group)?,
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

fn tool_config_read(args: &Value) -> Result<String> {
    let app = optional_string_arg(args, "app")?;
    let key = optional_string_arg(args, "key")?;
    if app.is_none() && key.is_some() {
        bail!("config_read \"key\" requires \"app\"");
    }
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
    // A missing or null value means "delete this key"; any other type is an
    // error, not an implicit deletion.
    let value = match args.get("value") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(other) => bail!(
            "config_write \"value\" must be a string or null, got {}",
            value_kind(other)
        ),
    };
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

fn ensure_window_targetable(window: &Window) -> Result<()> {
    if window.on_active_workspace == Some(false) {
        bail!(
            "{:?} is on an inactive workspace; its geometry does not describe the pixels currently \
             shown on that output. Activate the window or switch workspaces first.",
            window.title
        );
    }
    if window.state.iter().any(|state| state == "minimized") {
        bail!(
            "{:?} is minimized; its geometry is the last placement, not a visible target. \
             Unminimize it before sending pointer input.",
            window.title
        );
    }
    Ok(())
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

/// Captures one window without opening the EIS input session.
///
/// Direct window capture is preferred. The output-crop fallback needs a real
/// scale factor, so it is available only when an input session is already open;
/// otherwise the response asks the caller to run `prepare_input` explicitly.
fn tool_screenshot_window(args: &Value) -> Result<ToolResponse> {
    let needle = args
        .get("window")
        .and_then(Value::as_str)
        .context("screenshot_window needs \"window\" (identifier, app_id or title)")?;
    let scene = Scene::load(false)?;
    let index = scene.find_index(needle)?;
    let window = &scene.windows[index];
    let logical = window.logical.context(
        "the compositor has not reported this window's geometry yet; re-run list_windows",
    )?;
    let output_index = window
        .output
        .as_deref()
        .and_then(|name| output_index_for(name, &scene.outputs))
        .context("the window's output is not present in the current topology")?;
    let out = scene.output(output_index)?;
    let existing_geometry = scene.geoms[index].clone();

    let (mut direct, direct_error) = match capture::capture_window(&window.identifier) {
        Ok(frame) => (frame, None),
        Err(err) => (None, Some(format!("{err:#}"))),
    };
    let direct_size = direct.as_ref().map(|frame| (frame.width, frame.height));
    let direct_covers_window = direct.as_ref().is_some_and(|frame| {
        coords::direct_frame_covers_logical(logical, frame.width, frame.height)
    });

    if direct_covers_window {
        let frame = direct.take().expect("direct capture was checked above");
        let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
            .context("direct window frame did not match its declared size")?;
        let path = save_window_png(&image)?;
        let (pixels, scale) = existing_geometry
            .as_ref()
            .map_or((Value::Null, Value::Null), |g| {
                (json!(g.pixels), json!(g.region.scale))
            });
        let coordinates = json!({
            "window": window.identifier,
            "capture": "window",
            "image": {"width": frame.width, "height": frame.height},
            "logical": logical,
            "origin": Value::Null,
        });
        let (locked, lock_evidence, mut notes) = lock_notes();
        notes.push(
            "captured directly from the window source; occluding windows are not included"
                .to_string(),
        );
        if existing_geometry.is_none() {
            notes.push(
                "the input session was not opened. scale and pixels are null, but the coordinates \
                 object is complete; click_window resolves input coordinates when it acts"
                    .to_string(),
            );
        }
        notes.push(
            "pass the coordinates object unchanged to click_window / move_window / scroll_window \
             with a point from this image"
                .to_string(),
        );
        return Ok(ToolResponse::success(serde_json::to_string_pretty(
            &json!({
                "path": path.display().to_string(),
                "capture": {
                    "source": "window",
                    "direct": true,
                    "native_size": {"width": frame.width, "height": frame.height},
                    "coordinate_safe": true,
                },
                "input_session_opened": false,
                "side_effects": [],
                "image": {"width": frame.width, "height": frame.height},
                "coordinates": coordinates,
                "window": window_ref(window),
                "output": {
                    "index": output_index,
                    "name": window.output,
                    "size": {"width": out.width, "height": out.height},
                    "scale": scale,
                },
                "scale": scale,
                "logical": logical,
                "pixels": pixels,
                "origin": Value::Null,
                "workspaces": window.workspaces,
                "on_active_workspace": window.on_active_workspace,
                "screen_locked": locked,
                "lock_evidence": lock_evidence,
                "notes": notes,
                "summary": format!(
                    "{}\n{}x{} via window, window {:?}\npass the coordinates object unchanged to \
                     click_window / move_window / scroll_window",
                    path.display(),
                    frame.width,
                    frame.height,
                    window.title
                ),
            }),
        )?));
    }

    if existing_geometry.is_none() {
        let mut structured = json!({
            "status": "needs_input_session",
            "message": "direct window capture did not produce a coordinate-safe full-window image, \
                        and the output-crop fallback needs the EIS scale. This observation tool did \
                        not open the portal.",
            "window": window_ref(window),
            "output": {
                "index": output_index,
                "name": window.output,
                "size": {"width": out.width, "height": out.height},
            },
            "logical": logical,
            "capture": {
                "source": direct_size.map(|_| "window"),
                "native_size": direct_size.map(|(width, height)| {
                    json!({"width": width, "height": height})
                }).unwrap_or(Value::Null),
                "coordinate_safe": false,
            },
            "direct_error": direct_error,
            "next_action": "call prepare_input (a possible one-time system authorization dialog), \
                            then take a new screenshot_window",
        });
        if let Some(frame) = direct.take() {
            let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
                .context("direct window frame did not match its declared size")?;
            let path = save_window_png(&image)?;
            if let Some(object) = structured.as_object_mut() {
                object.insert("path".to_string(), json!(path.display().to_string()));
                object.insert(
                    "image".to_string(),
                    json!({"width": frame.width, "height": frame.height}),
                );
            }
        }
        return Ok(ToolResponse::error(structured));
    }

    let base = existing_geometry.context("existing input geometry disappeared")?;
    let shot = capture::capture(base.output_index)?;
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
            window.title, out.index, shot.width, shot.height
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
    let path = save_window_png(&cropped)?;
    let coordinates = json!({
        "window": window.identifier,
        "capture": "output_crop",
        "image": {"width": geometry.image.width, "height": geometry.image.height},
        "logical": geometry.logical,
        "origin": {"x": geometry.image.x, "y": geometry.image.y},
    });
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.extend(pointer_notes(window, &geometry));
    notes.push(
        "the output-crop fallback used an input session that was already open; screenshot_window \
         did not open it"
            .to_string(),
    );
    if let Some(err) = direct_error {
        notes.push(format!(
            "direct window capture was unavailable or unusable: {err}"
        ));
    }
    notes.push(
        "pass the coordinates object unchanged to click_window / move_window / scroll_window with \
         a point from this image"
            .to_string(),
    );
    Ok(ToolResponse::success(serde_json::to_string_pretty(
        &json!({
            "path": path.display().to_string(),
            "capture": {
                "source": "output_crop",
                "direct": false,
                "native_size": direct_size.map(|(width, height)| {
                    json!({"width": width, "height": height})
                }).unwrap_or(Value::Null),
                "coordinate_safe": true,
            },
            "input_session_opened": false,
            "side_effects": [],
            "image": {"width": geometry.image.width, "height": geometry.image.height},
            "coordinates": coordinates,
            "window": window_ref(window),
            "output": {
                "index": geometry.output_index,
                "name": geometry.output_name,
                "size": {"width": out.width, "height": out.height},
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
                "{}\n{}x{} via output_crop, window {:?}\npass the coordinates object unchanged to \
                 click_window / move_window / scroll_window",
                path.display(),
                geometry.image.width,
                geometry.image.height,
                window.title
            ),
        }),
    )?))
}

fn tool_window_action(args: &Value) -> Result<String> {
    let needle = args
        .get("window")
        .and_then(Value::as_str)
        .context("window_action needs \"window\" (identifier, app_id or title)")?;
    let action_name = args
        .get("action")
        .and_then(Value::as_str)
        .context("window_action needs \"action\"")?;
    let action = toplevel::Action::parse(action_name)?;
    let done = toplevel::act(needle, action)?;
    Ok(serde_json::to_string_pretty(&json!({
        "action": "window_action",
        "requested": action_name,
        "window": needle,
        "summary": done,
        "verified": false,
        "note": "the compositor may ignore this request; re-run list_windows to see the resulting state",
    }))?)
}

fn button_arg(args: &Value) -> Result<Button> {
    match args.get("button") {
        None | Some(Value::Null) => Ok(Button::Left),
        Some(Value::String(name)) => Button::parse(name),
        Some(other) => bail!(
            "button must be \"left\", \"right\" or \"middle\", got {}",
            value_kind(other)
        ),
    }
}

fn count_arg(args: &Value) -> Result<u32> {
    match optional_u64_arg(args, "count")?.unwrap_or(1) {
        1 => Ok(1),
        2 => Ok(2),
        count => bail!("count must be 1 or 2, got {count}"),
    }
}

fn window_arg<'a>(args: &'a Value, tool: &str) -> Result<&'a str> {
    args.get("window")
        .and_then(Value::as_str)
        .with_context(|| format!("{tool} needs \"window\" (identifier, app_id or title)"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowCaptureSpace {
    Window,
    OutputCrop,
}

#[derive(Clone, Debug)]
struct WindowCoordinates {
    window: String,
    capture: WindowCaptureSpace,
    image: (u32, u32),
    logical: Rect,
    origin: Option<(i32, i32)>,
}

fn window_coordinates_arg(args: &Value) -> Result<WindowCoordinates> {
    let coordinates = args
        .get("coordinates")
        .and_then(Value::as_object)
        .context("window input needs the \"coordinates\" object returned by screenshot_window")?;
    let window = coordinates
        .get("window")
        .and_then(Value::as_str)
        .context("coordinates.window is missing")?
        .to_string();
    let capture = match coordinates.get("capture").and_then(Value::as_str) {
        Some("window") => WindowCaptureSpace::Window,
        Some("output_crop") => WindowCaptureSpace::OutputCrop,
        Some(other) => {
            bail!("coordinates.capture is {other:?}; expected \"window\" or \"output_crop\"")
        }
        None => bail!("coordinates.capture is missing"),
    };
    let image = coordinates
        .get("image")
        .and_then(Value::as_object)
        .context("coordinates.image is missing")?;
    let width = image
        .get("width")
        .and_then(Value::as_u64)
        .context("coordinates.image.width is missing")?;
    let height = image
        .get("height")
        .and_then(Value::as_u64)
        .context("coordinates.image.height is missing")?;
    let width = u32::try_from(width).context("coordinates.image.width is out of range")?;
    let height = u32::try_from(height).context("coordinates.image.height is out of range")?;
    if width == 0 || height == 0 {
        bail!("coordinates.image must have non-zero width and height");
    }
    let logical = rect_from_json(
        coordinates
            .get("logical")
            .context("coordinates.logical is missing")?,
    )?;
    if logical.width <= 0 || logical.height <= 0 {
        bail!("coordinates.logical must have positive width and height");
    }
    let origin = coordinates
        .get("origin")
        .filter(|value| !value.is_null())
        .map(|value| -> Result<(i32, i32)> {
            let origin = value
                .as_object()
                .context("coordinates.origin must be an object or null")?;
            let coordinate = |name: &str| -> Result<i32> {
                let value = origin
                    .get(name)
                    .and_then(Value::as_i64)
                    .with_context(|| format!("coordinates.origin.{name} is missing"))?;
                i32::try_from(value)
                    .with_context(|| format!("coordinates.origin.{name} is out of range"))
            };
            Ok((coordinate("x")?, coordinate("y")?))
        })
        .transpose()?;
    if capture == WindowCaptureSpace::OutputCrop && origin.is_none() {
        bail!("coordinates.origin is required for capture=\"output_crop\"");
    }
    Ok(WindowCoordinates {
        window,
        capture,
        image: (width, height),
        logical,
        origin,
    })
}

fn rect_from_json(value: &Value) -> Result<Rect> {
    let rect = value.as_object().context("rect must be an object")?;
    let number = |name: &str| -> Result<i32> {
        let value = rect
            .get(name)
            .and_then(Value::as_i64)
            .with_context(|| format!("rect.{name} is missing"))?;
        i32::try_from(value).with_context(|| format!("rect.{name} is out of range"))
    };
    Ok(Rect {
        x: number("x")?,
        y: number("y")?,
        width: number("width")?,
        height: number("height")?,
    })
}

/// Resolves a point from a specific `screenshot_window` result into every
/// space, opening no EIS session itself; the caller has already loaded the
/// scene for an input action.
fn window_point(
    scene: &Scene,
    coordinates: &WindowCoordinates,
    x: i32,
    y: i32,
) -> Result<(usize, WindowGeometry, i32, i32, f64, f64)> {
    let index = scene.find_index(&coordinates.window)?;
    let window = &scene.windows[index];
    if window.logical != Some(coordinates.logical) {
        bail!(
            "the screenshot is stale: its logical window rect was {:?}, but the window is now at \
             {:?}; take a new screenshot_window",
            coordinates.logical,
            window.logical
        );
    }
    let geometry = scene.geometry(index)?.clone();
    let (ox, oy, lx, ly) = match coordinates.capture {
        WindowCaptureSpace::Window => {
            if !coords::direct_frame_covers_logical(
                coordinates.logical,
                coordinates.image.0,
                coordinates.image.1,
            ) {
                bail!(
                    "coordinates describe a direct window image that does not cover the whole \
                     window; take a new screenshot_window"
                );
            }
            let (ox, oy) =
                coords::direct_image_to_output(geometry.pixels, coordinates.image, x, y)?;
            let (lx, ly) = coords::direct_image_to_logical(
                &geometry.region,
                coordinates.logical,
                coordinates.image,
                x,
                y,
            )?;
            point_in_output(scene.output(geometry.output_index)?, ox, oy)?;
            (ox, oy, lx, ly)
        }
        WindowCaptureSpace::OutputCrop => {
            let origin = coordinates
                .origin
                .context("output_crop origin is missing")?;
            if coordinates.image
                != (
                    geometry.image.width.max(0) as u32,
                    geometry.image.height.max(0) as u32,
                )
                || origin != (geometry.image.x, geometry.image.y)
            {
                bail!(
                    "the screenshot is stale: its output crop was {}x{} at ({}, {}), but the \
                     current crop is {}x{} at ({}, {}); take a new screenshot_window",
                    coordinates.image.0,
                    coordinates.image.1,
                    origin.0,
                    origin.1,
                    geometry.image.width,
                    geometry.image.height,
                    geometry.image.x,
                    geometry.image.y
                );
            }
            let (ox, oy) = geometry.window_to_output(x, y)?;
            let (lx, ly) = geometry.window_to_logical(x, y)?;
            (ox, oy, lx, ly)
        }
    };
    Ok((index, geometry, ox, oy, lx, ly))
}

fn tool_click(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let button = button_arg(args)?;
    let count = count_arg(args)?;
    let output = output_arg(args)?;

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
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.push(INPUT_VERIFICATION_NOTE.to_string());

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
        "verified": false,
        "notes": notes,
    }))?)
}

fn tool_move(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let output = output_arg(args)?;

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
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.push(INPUT_VERIFICATION_NOTE.to_string());

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
        "verified": false,
        "notes": notes,
    }))?)
}

fn tool_scroll(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let dy = optional_i32_arg(args, "dy")?.unwrap_or(0);
    let dx = optional_i32_arg(args, "dx")?.unwrap_or(0);
    let output = output_arg(args)?;

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
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.push(INPUT_VERIFICATION_NOTE.to_string());

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
        "verified": false,
        "notes": notes,
    }))?)
}

fn tool_click_window(args: &Value) -> Result<String> {
    let needle = window_arg(args, "click_window")?;
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let button = button_arg(args)?;
    let count = count_arg(args)?;
    let coordinates = window_coordinates_arg(args)?;

    let windows = toplevel::list()?;
    let target = toplevel::find(&windows, needle)?;
    if target.identifier != coordinates.window {
        bail!(
            "the coordinates object belongs to window {:?}, not the requested window {:?}",
            coordinates.window,
            target.identifier
        );
    }
    ensure_window_targetable(&target)?;
    let scene = Scene::from_windows(windows, true)?;
    let (index, geometry, ox, oy, lx, ly) = window_point(&scene, &coordinates, x, y)?;
    let window = &scene.windows[index];
    let out = scene.output(geometry.output_index)?;
    let hit = scene.hit(geometry.output_index, ox, oy);
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.extend(pointer_notes(window, &geometry));
    notes.push(INPUT_VERIFICATION_NOTE.to_string());

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
        "image": {"width": coordinates.image.0, "height": coordinates.image.1},
        "coordinates": {
            "capture": match coordinates.capture {
                WindowCaptureSpace::Window => "window",
                WindowCaptureSpace::OutputCrop => "output_crop",
            },
            "image": {"width": coordinates.image.0, "height": coordinates.image.1},
            "logical": coordinates.logical,
            "origin": coordinates.origin.map(|(x, y)| json!({"x": x, "y": y})).unwrap_or(Value::Null),
        },
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
        "verified": false,
        "notes": notes,
    }))?)
}

fn tool_move_window(args: &Value) -> Result<String> {
    let needle = window_arg(args, "move_window")?;
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let coordinates = window_coordinates_arg(args)?;

    let windows = toplevel::list()?;
    let target = toplevel::find(&windows, needle)?;
    if target.identifier != coordinates.window {
        bail!(
            "the coordinates object belongs to window {:?}, not the requested window {:?}",
            coordinates.window,
            target.identifier
        );
    }
    ensure_window_targetable(&target)?;
    let scene = Scene::from_windows(windows, true)?;
    let (index, geometry, ox, oy, lx, ly) = window_point(&scene, &coordinates, x, y)?;
    let window = &scene.windows[index];
    let out = scene.output(geometry.output_index)?;
    let hit = scene.hit(geometry.output_index, ox, oy);
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.extend(pointer_notes(window, &geometry));
    notes.push(INPUT_VERIFICATION_NOTE.to_string());

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
        "image": {"width": coordinates.image.0, "height": coordinates.image.1},
        "coordinates": {
            "capture": match coordinates.capture {
                WindowCaptureSpace::Window => "window",
                WindowCaptureSpace::OutputCrop => "output_crop",
            },
            "image": {"width": coordinates.image.0, "height": coordinates.image.1},
            "logical": coordinates.logical,
            "origin": coordinates.origin.map(|(x, y)| json!({"x": x, "y": y})).unwrap_or(Value::Null),
        },
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
        "verified": false,
        "notes": notes,
    }))?)
}

fn tool_scroll_window(args: &Value) -> Result<String> {
    let needle = window_arg(args, "scroll_window")?;
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let dy = optional_i32_arg(args, "dy")?.unwrap_or(0);
    let dx = optional_i32_arg(args, "dx")?.unwrap_or(0);
    let coordinates = window_coordinates_arg(args)?;

    let windows = toplevel::list()?;
    let target = toplevel::find(&windows, needle)?;
    if target.identifier != coordinates.window {
        bail!(
            "the coordinates object belongs to window {:?}, not the requested window {:?}",
            coordinates.window,
            target.identifier
        );
    }
    ensure_window_targetable(&target)?;
    let scene = Scene::from_windows(windows, true)?;
    let (index, geometry, ox, oy, lx, ly) = window_point(&scene, &coordinates, x, y)?;
    let window = &scene.windows[index];
    let out = scene.output(geometry.output_index)?;
    let hit = scene.hit(geometry.output_index, ox, oy);
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.extend(pointer_notes(window, &geometry));
    notes.push(INPUT_VERIFICATION_NOTE.to_string());

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
        "image": {"width": coordinates.image.0, "height": coordinates.image.1},
        "coordinates": {
            "capture": match coordinates.capture {
                WindowCaptureSpace::Window => "window",
                WindowCaptureSpace::OutputCrop => "output_crop",
            },
            "image": {"width": coordinates.image.0, "height": coordinates.image.1},
            "logical": coordinates.logical,
            "origin": coordinates.origin.map(|(x, y)| json!({"x": x, "y": y})).unwrap_or(Value::Null),
        },
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
        "verified": false,
        "notes": notes,
    }))?)
}

fn tool_type_text(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .context("missing text")?;
    if text.is_empty() {
        let (locked, lock_evidence, notes) = lock_notes();
        return Ok(serde_json::to_string_pretty(&json!({
            "action": "type_text",
            "characters": 0,
            "screen_locked": locked,
            "lock_evidence": lock_evidence,
            "verified": true,
            "notes": notes,
        }))?);
    }

    let mode = optional_string_arg(args, "mode")?.unwrap_or("paste");
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.push(INPUT_VERIFICATION_NOTE.to_string());
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
            Ok(serde_json::to_string_pretty(&json!({
                "action": "type_text",
                "mode": "keys",
                "characters": text.chars().count(),
                "clipboard_overwritten": false,
                "screen_locked": locked,
                "lock_evidence": lock_evidence,
                "verified": false,
                "notes": notes,
            }))?)
        }
        "paste" => {
            clipboard::write(text)?;
            // The compositor needs to see the new selection before the paste
            // request, otherwise the target reads the previous contents.
            std::thread::sleep(Duration::from_millis(120));
            press_chord("ctrl+v")
                .context("the clipboard was overwritten, but the ctrl+v input request failed")?;
            notes.push("the clipboard now holds this text".to_string());
            Ok(serde_json::to_string_pretty(&json!({
                "action": "type_text",
                "mode": "paste",
                "characters": text.chars().count(),
                "clipboard_overwritten": true,
                "screen_locked": locked,
                "lock_evidence": lock_evidence,
                "verified": false,
                "notes": notes,
            }))?)
        }
        other => bail!("unknown mode {other:?}; expected \"paste\" or \"keys\""),
    }
}

fn tool_key(args: &Value) -> Result<String> {
    let combo = args
        .get("combo")
        .and_then(Value::as_str)
        .context("missing combo")?;
    let (locked, lock_evidence, mut notes) = lock_notes();
    notes.push(INPUT_VERIFICATION_NOTE.to_string());
    press_chord(combo)?;
    Ok(serde_json::to_string_pretty(&json!({
        "action": "key",
        "combo": combo,
        "screen_locked": locked,
        "lock_evidence": lock_evidence,
        "verified": false,
        "notes": notes,
    }))?)
}

fn press_chord(combo: &str) -> Result<()> {
    input::chord(combo)
}

fn output_arg(args: &Value) -> Result<usize> {
    let output = optional_u64_arg(args, "output")?.unwrap_or(0);
    usize::try_from(output).context("output index is out of range")
}

fn optional_string_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(other) => bail!(
            "argument {key:?} must be a string, got {}",
            value_kind(other)
        ),
    }
}

fn optional_u64_arg(args: &Value, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).with_context(|| {
            format!(
                "argument {key:?} must be a non-negative integer, got {}",
                value_kind(value)
            )
        }),
    }
}

fn optional_i32_arg(args: &Value, key: &str) -> Result<Option<i32>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let value = value.as_i64().with_context(|| {
                format!(
                    "argument {key:?} must be an integer, got {}",
                    value_kind(value)
                )
            })?;
            Ok(Some(i32::try_from(value).with_context(|| {
                format!("argument {key:?} is out of range")
            })?))
        }
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn int_arg(args: &Value, key: &str) -> Result<i32> {
    let value = args
        .get(key)
        .and_then(Value::as_i64)
        .with_context(|| format!("missing integer argument {key:?}"))?;
    i32::try_from(value).with_context(|| format!("argument {key:?} out of range"))
}

fn screenshot_dir_at(runtime_dir: &Path) -> Result<PathBuf> {
    let dir = runtime_dir.join("cosmic-desktop-mcp");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, Permissions::from_mode(0o700))
        .with_context(|| format!("make {} private", dir.display()))?;
    Ok(dir)
}

fn screenshot_dir() -> Result<PathBuf> {
    screenshot_dir_at(&crate::session::runtime_dir()?)
}

fn save_png_at(dir: &Path, prefix: &str, image: &image::RgbaImage) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("{prefix}-{stamp}.png"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    let encoded = PngEncoder::new(&mut file)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        )
        .with_context(|| format!("encode {}", path.display()))
        .and_then(|()| {
            file.sync_all()
                .with_context(|| format!("sync {}", path.display()))
        });
    if let Err(err) = encoded {
        let _ = std::fs::remove_file(&path);
        return Err(err);
    }
    Ok(path)
}

fn save_png(image: &image::RgbaImage, prefix: &str) -> Result<PathBuf> {
    save_png_at(&screenshot_dir()?, prefix, image)
}

fn save_window_png(image: &image::RgbaImage) -> Result<PathBuf> {
    save_png(image, "window")
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
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cosmic-desktop-mcp-artifacts-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
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
                "prepare_input",
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
                "key",
                "workspaces",
                "applets",
                "restart_applet",
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
        let success = tool_call_result(Ok(ToolResponse::success(
            json!({"ok": true, "count": 2}).to_string(),
        )));
        assert_eq!(success.is_error, Some(false));
        assert_eq!(
            success.content[0].as_text().map(|text| text.text.as_str()),
            Some(r#"{"count":2,"ok":true}"#)
        );
        assert_eq!(
            success.structured_content,
            Some(json!({"ok": true, "count": 2}))
        );

        let failure = tool_call_result(Err(anyhow::anyhow!("tool failed: detail")));
        assert_eq!(failure.is_error, Some(true));
        assert_eq!(
            failure.content[0].as_text().map(|text| text.text.as_str()),
            Some("tool failed: detail")
        );
        assert!(failure.structured_content.is_none());
    }

    #[test]
    fn authorization_errors_are_structured_for_the_client() {
        let timeout = tool_call_result(Err(anyhow::anyhow!(
            "open an EIS session: the EIS session did not come up within 60s"
        )));
        assert_eq!(timeout.is_error, Some(true));
        assert_eq!(
            timeout
                .structured_content
                .as_ref()
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str),
            Some("authorization_timeout")
        );

        let denied = tool_call_result(Err(anyhow::anyhow!(
            "the portal granted no input devices — authorization was probably denied"
        )));
        assert_eq!(
            denied
                .structured_content
                .as_ref()
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str),
            Some("authorization_denied")
        );

        let unavailable = tool_call_result(Err(anyhow::anyhow!(
            "connect to the RemoteDesktop portal: service unavailable"
        )));
        assert_eq!(
            unavailable
                .structured_content
                .as_ref()
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str),
            Some("authorization_unavailable")
        );
    }

    #[test]
    fn structured_tool_errors_survive_without_text_classification() {
        let result = tool_call_result(Ok(ToolResponse::error(json!({
            "status": "needs_input_session",
            "next_action": "call prepare_input"
        }))));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str),
            Some("needs_input_session")
        );
    }

    #[test]
    fn invalid_arguments_are_errors_instead_of_silent_defaults() {
        assert_eq!(output_arg(&json!({})).unwrap(), 0);
        assert!(output_arg(&json!({"output": -1})).is_err());
        assert!(output_arg(&json!({"output": "0"})).is_err());

        assert_eq!(count_arg(&json!({})).unwrap(), 1);
        assert_eq!(count_arg(&json!({"count": 2})).unwrap(), 2);
        assert!(count_arg(&json!({"count": 3})).is_err());
        assert!(count_arg(&json!({"count": "1"})).is_err());

        assert!(button_arg(&json!({"button": 1})).is_err());
        assert!(optional_i32_arg(&json!({"dy": "1"}), "dy").is_err());
        assert!(optional_string_arg(&json!({"mode": 1}), "mode").is_err());

        let err = tool_config_write(&json!({
            "app": "com.system76.CosmicAppletTime",
            "key": "military_time",
            "value": 1
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("must be a string or null"), "{err}");

        let err = tool_type_text(&json!({"text": "x", "mode": 1}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("mode"), "{err}");

        let err = tool_workspaces(&json!({"group": 1}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires \"activate\""), "{err}");

        let err = tool_config_read(&json!({"key": "military_time"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires \"app\""), "{err}");
    }

    #[test]
    fn screenshot_artifacts_are_private() {
        let temp = TempDir::new();
        let dir = screenshot_dir_at(&temp.0).unwrap();
        let path = save_png_at(&dir, "test", &image::RgbaImage::new(1, 1)).unwrap();

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn pointer_input_rejects_windows_that_are_not_visible_targets() {
        let inactive = Window {
            title: "inactive".into(),
            on_active_workspace: Some(false),
            ..Window::default()
        };
        let minimized = Window {
            title: "minimized".into(),
            state: vec!["minimized".into()],
            ..Window::default()
        };

        assert!(ensure_window_targetable(&inactive).is_err());
        assert!(ensure_window_targetable(&minimized).is_err());
        assert!(ensure_window_targetable(&Window::default()).is_ok());
    }

    #[test]
    fn window_coordinates_parse_direct_and_output_crop_shapes() {
        let direct = window_coordinates_arg(&json!({
            "coordinates": {
                "window": "abc",
                "capture": "window",
                "image": {"width": 1280, "height": 720},
                "logical": {"x": 10, "y": 20, "width": 640, "height": 360},
                "origin": null
            }
        }))
        .unwrap();
        assert_eq!(direct.window, "abc");
        assert_eq!(direct.capture, WindowCaptureSpace::Window);
        assert_eq!(direct.image, (1280, 720));
        assert_eq!(direct.origin, None);

        let crop = window_coordinates_arg(&json!({
            "coordinates": {
                "window": "abc",
                "capture": "output_crop",
                "image": {"width": 960, "height": 540},
                "logical": {"x": 10, "y": 20, "width": 640, "height": 360},
                "origin": {"x": 15, "y": 30}
            }
        }))
        .unwrap();
        assert_eq!(crop.capture, WindowCaptureSpace::OutputCrop);
        assert_eq!(crop.origin, Some((15, 30)));
    }

    #[test]
    fn window_coordinates_reject_incomplete_or_ambiguous_shapes() {
        let missing_origin = window_coordinates_arg(&json!({
            "coordinates": {
                "window": "abc",
                "capture": "output_crop",
                "image": {"width": 960, "height": 540},
                "logical": {"x": 10, "y": 20, "width": 640, "height": 360}
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(
            missing_origin.contains("origin is required"),
            "{missing_origin}"
        );

        let incomplete_origin = window_coordinates_arg(&json!({
            "coordinates": {
                "window": "abc",
                "capture": "output_crop",
                "image": {"width": 960, "height": 540},
                "logical": {"x": 10, "y": 20, "width": 640, "height": 360},
                "origin": {"x": 15}
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(
            incomplete_origin.contains("origin.y is missing"),
            "{incomplete_origin}"
        );

        let bad_capture = window_coordinates_arg(&json!({
            "coordinates": {
                "window": "abc",
                "capture": "surface",
                "image": {"width": 960, "height": 540},
                "logical": {"x": 10, "y": 20, "width": 640, "height": 360}
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(bad_capture.contains("expected \"window\""), "{bad_capture}");
    }

    fn coordinate_scene() -> Scene {
        let logical = Rect {
            x: 10,
            y: 20,
            width: 640,
            height: 360,
        };
        let region = crate::eis::Region {
            x: 384.0,
            y: 1152.0,
            width: 1280,
            height: 720,
            scale: 1.5,
            mapping_id: Some("eDP-1".into()),
        };
        let output = capture::OutputInfo {
            index: 0,
            name: Some("eDP-1".into()),
            width: 1920,
            height: 1080,
            scale: 2,
        };
        let window = Window {
            identifier: "window-id".into(),
            title: "test window".into(),
            app_id: "test.app".into(),
            output: Some("eDP-1".into()),
            logical: Some(logical),
            ..Window::default()
        };
        let geometry = WindowGeometry::new(
            region.clone(),
            0,
            Some("eDP-1".into()),
            logical,
            (output.width, output.height),
        )
        .unwrap();
        Scene {
            windows: vec![window],
            outputs: vec![output],
            regions: vec![region],
            geoms: vec![Some(geometry)],
        }
    }

    #[test]
    fn direct_window_points_use_the_screenshot_dimensions() {
        let scene = coordinate_scene();
        let coordinates = window_coordinates_arg(&json!({
            "coordinates": {
                "window": "window-id",
                "capture": "window",
                "image": {"width": 1280, "height": 720},
                "logical": {"x": 10, "y": 20, "width": 640, "height": 360},
                "origin": null
            }
        }))
        .unwrap();
        let (_, _, ox, oy, lx, ly) = window_point(&scene, &coordinates, 640, 360).unwrap();
        assert_eq!((ox, oy), (495, 300));
        assert_eq!((lx, ly), (714.0, 1352.0));
    }

    #[test]
    fn direct_window_points_reject_incomplete_captures() {
        let scene = coordinate_scene();
        let coordinates = window_coordinates_arg(&json!({
            "coordinates": {
                "window": "window-id",
                "capture": "window",
                "image": {"width": 1280, "height": 696},
                "logical": {"x": 10, "y": 20, "width": 640, "height": 360},
                "origin": null
            }
        }))
        .unwrap();
        let err = window_point(&scene, &coordinates, 640, 348)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not cover the whole window"), "{err}");
    }

    #[test]
    fn output_crop_points_reject_a_stale_origin() {
        let scene = coordinate_scene();
        let coordinates = window_coordinates_arg(&json!({
            "coordinates": {
                "window": "window-id",
                "capture": "output_crop",
                "image": {"width": 960, "height": 540},
                "logical": {"x": 10, "y": 20, "width": 640, "height": 360},
                "origin": {"x": 99, "y": 30}
            }
        }))
        .unwrap();
        let err = window_point(&scene, &coordinates, 10, 10)
            .unwrap_err()
            .to_string();
        assert!(err.contains("screenshot is stale"), "{err}");
    }
}

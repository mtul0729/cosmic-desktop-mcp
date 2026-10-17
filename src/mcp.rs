//! Minimal MCP server over stdio.
//!
//! Hand-rolled JSON-RPC rather than an SDK dependency: the wire surface an
//! agent needs is `initialize` / `tools/list` / `tools/call`, and keeping the
//! dependency tree small is what makes the crane `cargoArtifacts` cheap to
//! cache while the tools are still being iterated on.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::applets;
use crate::clipboard;
use crate::config;
use crate::input;
use crate::keymap::Button;
use crate::wl::{capture, toplevel, workspace};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line.context("read stdin")?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("cosmic-desktop-mcp: bad JSON: {err}");
                continue;
            }
        };

        // Notifications carry no id and must not be answered.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let response = match dispatch(method, request.get("params")) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": format!("{err:#}")}
            }),
        };

        serde_json::to_writer(&mut stdout, &response).context("encode response")?;
        stdout.write_all(b"\n").context("write newline")?;
        stdout.flush().context("flush stdout")?;
    }

    Ok(())
}

fn dispatch(method: &str, params: Option<&Value>) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "cosmic-desktop-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => call_tool(params),
        other => bail!("unsupported method {other:?}"),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "screenshot",
            "description": "Capture a display and save it as a PNG. Returns the file path so the \
caller can open it. Coordinates accepted by click/move/scroll are in the captured image's pixel \
space, with no downscaling applied.",
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
            "description": "List open toplevel windows with a stable identifier, title and app_id. \
Window state (focused/minimized) is not available on COSMIC.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "click",
            "description": "Click a point given in screenshot pixel coordinates. Moves the pointer \
first, waits for the compositor to register the hover, then presses and releases.",
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
            "description": "Move the pointer without clicking (hover), in screenshot pixel coordinates.",
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
            "description": "Scroll the wheel at a point. Positive dy scrolls UP (reveals earlier \
content, matching the kernel REL_WHEEL convention), negative dy scrolls down; positive dx \
scrolls right.",
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
    ])
}

fn call_tool(params: Option<&Value>) -> Result<Value> {
    let params = params.context("tools/call without params")?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("tools/call without name")?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let outcome = match name {
        "screenshot" => tool_screenshot(&args),
        "env_info" => tool_env_info(),
        "list_windows" => tool_list_windows(),
        "click" => tool_click(&args),
        "move" => tool_move(&args),
        "scroll" => tool_scroll(&args),
        "type_text" => tool_type_text(&args),
        "clipboard_write" => tool_clipboard_write(&args),
        "clipboard_read" => tool_clipboard_read(),
        "key" => tool_key(&args),
        "workspaces" => tool_workspaces(&args),
        "applets" => tool_applets(),
        "restart_applet" => tool_restart_applet(&args),
        "app_logs" => tool_app_logs(&args),
        "config_read" => tool_config_read(&args),
        "config_write" => tool_config_write(&args),
        other => bail!("unknown tool {other:?}"),
    };

    match outcome {
        Ok(text) => Ok(json!({
            "content": [{"type": "text", "text": text}],
            "isError": false
        })),
        Err(err) => Ok(json!({
            "content": [{"type": "text", "text": format!("{err:#}")}],
            "isError": true
        })),
    }
}

fn tool_screenshot(args: &Value) -> Result<String> {
    let index = args.get("output").and_then(Value::as_u64).unwrap_or(0) as usize;
    let shot = capture::capture(index)?;

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

    Ok(format!(
        "{}\n{}x{} output_index={} (coordinates are in this image's pixel space)",
        path.display(),
        shot.width,
        shot.height,
        shot.output.index
    ))
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
        "click/move/scroll take pixel coordinates of a screenshot of the chosen output; the \
         conversion to logical desktop coordinates happens inside the tool."
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
        "protocol_version": PROTOCOL_VERSION,
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
    Ok(serde_json::to_string_pretty(&config::write(app, key, value)?)?)
}

fn tool_list_windows() -> Result<String> {
    let windows = toplevel::list()?;
    Ok(serde_json::to_string_pretty(&windows)?)
}

fn tool_click(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let button = match args.get("button").and_then(Value::as_str) {
        Some(name) => Button::parse(name)?,
        None => Button::Left,
    };
    let count = args.get("count").and_then(Value::as_u64).unwrap_or(1).max(1);
    let output = output_arg(args);

    input::click(button, x, y, output, count as u32)?;
    Ok(format!(
        "clicked {button:?} at ({x}, {y}) on output {output} x{count}"
    ))
}

fn tool_move(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let output = output_arg(args);
    input::move_to(x, y, output)?;
    Ok(format!("pointer at ({x}, {y}) on output {output}"))
}

fn tool_scroll(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let dy = args.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32;
    let dx = args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;

    let output = output_arg(args);
    input::scroll(x, y, output, dx, dy)?;
    Ok(format!("scrolled dx={dx} dy={dy} at ({x}, {y}) on output {output}"))
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
        // is treated as a pinyin syllable and commits as a Chinese candidate
        // ("abc123" arrived as 按扁成23). So keycodes are opt-in, and only for
        // callers that know the input method is off or that the target wants
        // real key events.
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
    Ok(format!("clipboard set ({} characters)", text.chars().count()))
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
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `--doctor` output: proves the three moving parts work without running a
/// tool call against a live desktop.
pub fn doctor() -> Result<()> {
    println!("wayland: {:?}", std::env::var("WAYLAND_DISPLAY"));
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
        println!("  {} [{}] {}", window.app_id, window.identifier, window.title);
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

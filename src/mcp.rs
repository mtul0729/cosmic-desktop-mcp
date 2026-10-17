//! Minimal MCP server over stdio.
//!
//! Hand-rolled JSON-RPC rather than an SDK dependency: the wire surface an
//! agent needs is `initialize` / `tools/list` / `tools/call`, and keeping the
//! dependency tree small is what makes the crane `cargoArtifacts` cheap to
//! cache while the tools are still being iterated on.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::uinput::{Button, Device, parse_chord};
use crate::wl::{capture, toplevel};

const PROTOCOL_VERSION: &str = "2024-11-05";

static DEVICE: OnceLock<Mutex<Device>> = OnceLock::new();

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
                    }
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
                    "y": {"type": "integer"}
                },
                "required": ["x", "y"]
            }
        },
        {
            "name": "scroll",
            "description": "Scroll the wheel at a point. Positive dy scrolls down, positive dx scrolls right.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "dy": {"type": "integer", "description": "Wheel clicks, vertical."},
                    "dx": {"type": "integer", "description": "Wheel clicks, horizontal."}
                },
                "required": ["x", "y"]
            }
        },
        {
            "name": "type_text",
            "description": "Type printable ASCII into the focused window. Non-ASCII (e.g. Chinese) \
is rejected: it needs an input method, which a uinput device cannot drive.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
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
        "list_windows" => tool_list_windows(),
        "click" => tool_click(&args),
        "move" => tool_move(&args),
        "scroll" => tool_scroll(&args),
        "type_text" => tool_type_text(&args),
        "key" => tool_key(&args),
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

    let device = device()?;
    device.move_to(x, y)?;
    // A press that lands during the move is treated as the end of a drag; the
    // compositor needs to see a settled hover first.
    std::thread::sleep(Duration::from_millis(250));

    for round in 0..count {
        if round > 0 {
            std::thread::sleep(Duration::from_millis(60));
        }
        device.button(button, true)?;
        std::thread::sleep(Duration::from_millis(90));
        device.button(button, false)?;
    }

    Ok(format!("clicked {button:?} at ({x}, {y}) x{count}"))
}

fn tool_move(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    device()?.move_to(x, y)?;
    Ok(format!("pointer at ({x}, {y})"))
}

fn tool_scroll(args: &Value) -> Result<String> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    let dy = args.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32;
    let dx = args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;

    let device = device()?;
    device.move_to(x, y)?;
    std::thread::sleep(Duration::from_millis(150));
    device.scroll(dy, dx)?;
    Ok(format!("scrolled dx={dx} dy={dy} at ({x}, {y})"))
}

fn tool_type_text(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .context("missing text")?;
    device()?.type_text(text)?;
    Ok(format!("typed {} characters", text.chars().count()))
}

fn tool_key(args: &Value) -> Result<String> {
    let combo = args
        .get("combo")
        .and_then(Value::as_str)
        .context("missing combo")?;
    let codes = parse_chord(combo)?;

    let device = device()?;
    for code in &codes {
        device.key(*code, true)?;
    }
    for code in codes.iter().rev() {
        device.key(*code, false)?;
    }
    Ok(format!("pressed {combo}"))
}

fn int_arg(args: &Value, key: &str) -> Result<i32> {
    let value = args
        .get(key)
        .and_then(Value::as_i64)
        .with_context(|| format!("missing integer argument {key:?}"))?;
    i32::try_from(value).with_context(|| format!("argument {key:?} out of range"))
}

fn device() -> Result<std::sync::MutexGuard<'static, Device>> {
    if DEVICE.get().is_none() {
        let outputs = capture::outputs().context("probe outputs for pointer range")?;
        let main = outputs.first().context("no output to map coordinates to")?;
        let opened = Device::open("cosmic-desktop-mcp", main.width, main.height)?;
        let _ = DEVICE.set(Mutex::new(opened));
    }
    DEVICE
        .get()
        .context("device init raced")?
        .lock()
        .map_err(|err| anyhow::anyhow!("uinput device lock poisoned: {err}"))
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
            "output #{}: {}x{} scale={}",
            output.index, output.width, output.height, output.scale
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
    let main = outputs.first().context("no output")?;
    let device = Device::open("cosmic-desktop-mcp", main.width, main.height)
        .context("open /dev/uinput")?;
    println!("uinput: ok ({}x{})", device.size().0, device.size().1);
    Ok(())
}

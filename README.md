# cosmic-desktop-mcp

An MCP server that lets an agent see and drive a COSMIC desktop: screenshots, window
listing, mouse clicks, keyboard input.

Runs natively on Wayland using the protocols COSMIC actually implements — no X11. The
only portal use is the one-time RemoteDesktop authorization that input needs.

## Why write this yourself

Every public Linux desktop MCP is either X11-era (xdotool + scrot) or bound to a GNOME
Shell extension. None of that holds on COSMIC:

| Common approach | On COSMIC |
|---|---|
| `xdotool` / `scrot` | Only reaches XWayland clients; native windows are unreachable |
| `grim` / `wayshot` (wlr-screencopy) | COSMIC does not implement the protocol; fails outright |
| `cosmic-screenshot` non-interactive mode | Hangs (the portal never answers, 30s timeout) |
| `gnome-desktop-mcp` | Needs a GNOME Shell 45-49 extension; irrelevant |
| `ydotool` | Relative pointer plus acceleration; long jumps drift |

Measured: `cosmic-comp 1.9.0` exposes 62 Wayland globals, and three of them are enough:

- **Screenshots** — `ext_image_copy_capture_manager_v1` (staging), no dialog
- **Windows** — `ext_foreign_toplevel_list_v1` (staging), the same data `lswt` shows
- **Keyboard and mouse** — EIS (libei), authorized through the portal's RemoteDesktop v2:
  coordinates are **logical pixels** spanning all outputs, so mixed scale factors are a
  non-issue. The cost is one authorization dialog per machine (a restore token makes it
  silent afterwards). See "The EIS backend" below.
  An earlier version synthesized kernel devices through `/dev/uinput`: it needed device
  permissions, built its absolute coordinate space per output in physical pixels, and got
  both multiple monitors and mixed scaling wrong. Removed wholesale on 2026-10-17; there
  is no fallback switch.

## Status (measured 2026-10-17, pointer/workspace surface extended 2026-10-22)

| Capability | Status |
|---|---|
| `list_windows` | ✅ works, with state, geometry and workspace membership (see "Window geometry" for the measurements) |
| `screenshot_window` | ✅ works; the crop is exact at DP-2's 1.25 scale, and the response carries the same geometry fields as `list_windows`. An occluded window needs `activate` first |
| `window_action` | ✅ activate/close/maximize/minimize/fullscreen all take effect; the state array follows |
| Pointer and keyboard (`click` / `move` / `scroll` / `click_window` / `move_window` / `scroll_window` / `type_text` / `key`) | ✅ over EIS (libei), no `/dev/uinput` needed. Every pointer call reports the output used and the window under the point |
| Scroll direction | ✅ measured: positive `dy` scrolls up, negative down (the `REL_WHEEL` convention) |
| `screenshot` | ✅ works, colours and orientation correct; reports `screen_locked` from logind so a greeter frame is not mistaken for the desktop |

**One behaviour worth knowing:** while the screen is locked (cosmic-greeter running),
COSMIC suspends capture — `create_session` succeeds, but `capture()` on a frame never
returns an event until the screen is unlocked. That is reasonable (a lock screen should
not be photographed), but the MCP call waits out its 5-second timeout. While chasing
this I first misdiagnosed it as "the shm path is unimplemented"; the screen was simply
locked.

## Tools

| Tool | What it does | Built on |
|---|---|---|
| `screenshot` | Capture one output, save a PNG, return the path and `screen_locked` | ext-image-copy-capture + wl_shm |
| `env_info` | Desktop topology + input backend status (no side effects, opens no EIS session) | wl_output + EIS region |
| `list_windows` | Window list + state + **geometry** (`logical` and screenshot-pixel coordinates) + workspace membership | ext-foreign-toplevel-list + zcosmic-toplevel-info |
| `screenshot_window` | Find the window's output, capture it, crop the window, return its geometry in all three spaces | capture + zcosmic-toplevel-info + EIS region |
| `window_action` | activate / close / maximize / minimize / fullscreen and friends | zcosmic-toplevel-manager |
| `click` | Click at **output screenshot** coordinates; reports the output used and the window under the point | EIS |
| `move` | Hover at output screenshot coordinates | EIS |
| `scroll` | Wheel at output screenshot coordinates; positive `dy` up, negative down | EIS |
| `click_window` | Click at **window image** coordinates; resolves the window's output and crop origin itself | EIS + zcosmic-toplevel-info |
| `move_window` | Hover at window image coordinates | EIS + zcosmic-toplevel-info |
| `scroll_window` | Wheel at window image coordinates | EIS + zcosmic-toplevel-info |
| `type_text` | **Clipboard + ctrl+v by default**; `mode:"keys"` sends keycodes instead | wl-clipboard + EIS |
| `key` | Chord such as `ctrl+s` | EIS |
| `clipboard_write` | Put text on the clipboard without pasting | wl-copy (from wl-clipboard) |
| `clipboard_read` | Read the clipboard | wl-paste |
| `workspaces` | List / switch workspaces (group + name + state) | ext-workspace-manager |
| `applets` | List panel applet processes: name / PID / parent / memory / age | /proc |
| `restart_applet` | Refresh a panel applet (rewrites the `plugins_*` config) | cosmic-panel config files |
| `app_logs` | Read one applet's journal output | journalctl |
| `config_read` | Read a cosmic-config key (RON) | `~/.config/cosmic` |
| `config_write` | Write or delete a cosmic-config key, returning the old value for rollback | `~/.config/cosmic` |

## Coordinate spaces

Three spaces appear in responses, and mixing them is the most expensive mistake this server
can make. It happened: a window on output 1 had a crop origin of (966,60) in that output's
screenshot, the caller added it to a window-image point and called `click`, which defaults
to output 0. Every click landed on the other monitor while the tool answered "clicked ... on
output 0". It cost an hour.

| Space | Where it comes from | Who accepts it |
|---|---|---|
| **logical** | The compositor's desktop coordinates. A window's `logical` rect is relative to its output; EIS absolute input uses desktop-wide logical coordinates. | No MCP tool. `--eis-click` takes it. Everything else converts for you. |
| **output screenshot pixels** | The PNG from `screenshot {output}`, at that output's physical resolution. | `click` / `move` / `scroll`, with the **same** `output` index. |
| **window image pixels** | The PNG from `screenshot_window {window}`, a crop of the output at the window's origin. | `click_window` / `move_window` / `scroll_window`. |

The conversions are:

- window image -> output screenshot: add the crop `origin`
- output screenshot -> logical: divide by the output's real scale (from the EIS region,
  never `wl_output.scale`) and add the region's logical origin

`click` defaults to output 0 and **does not infer the output from x/y**. If the point came
from `screenshot_window`, use `click_window`: it resolves the window's output and crop origin
internally, so there is no output index to get wrong and no arithmetic to do. That is the
rule, not a convenience.

Every pointer call now reports what it actually did: the output used, the logical point, and
the window under the point (or null for the desktop), plus the workspace visibility of that
window. When the point is empty on the requested output but falls inside a window on another
output, the reply carries `other_output_match` with a warning — the exact signature of the
output-index mistake.

Window image pixels are a crop of the output screenshot, not the client's surface buffer:
COSMIC's composited output is what the capture protocol returns, so the window image has the
output's scale. `screenshot_window` reports `scale`, `logical`, `pixels` and `origin`, which
is everything needed to see that without joining it to `list_windows`.

The **workflow** an agent needs (run `env_info` first, `activate` before typing, never
`kill` an applet — the ordering rules and the traps) is deliberately not in the tool
descriptions. It ships in this repository as an agent skill,
[`skill/cosmic-desktop-ops`](skill/cosmic-desktop-ops/SKILL.md). A tool schema only says
what one call does, and every `tools/list` pays for that text; knowing which call comes
next and what will bite you is knowledge worth loading on demand only.

The skill lives here rather than in a skills collection because it is the manual for *this*
API: rename a tool or fix a quirk and it must change in the same commit. Install it through
Magpie with source `mtul0729/cosmic-desktop-mcp`, path `skill/cosmic-desktop-ops`.

## COSMIC-specific operations

Driving the desktop is one use; debugging COSMIC's own applications, widgets and applets
is the other. Everything below was measured on this machine (COSMIC 1.9.0) and shaped the
tools.

### An applet is a process, but the panel does not supervise it

Each applet is its own process, started by `cosmic-panel`, one set per monitor.
`cosmic-panel`'s source does build a launch-pad `ProcessManager` with
`RestartMode::ExponentialBackoff`, but **after a SIGTERM the panel does not bring the
applet back** — the slot just stays empty.

The only restart that was observed to work is **rewriting the panel's `plugins_*`
config**: write `None` into the key that lists the applet, let the config watcher see it,
then write the original text back. The panel rebuilds the whole section, so
`restart_applet` is really "refresh the panel" — time, workspaces and audio on that panel
all come back with new PIDs. The tool always restores the original text and keeps a
backup in `~/.local/state/cosmic-desktop-mcp/panel-backups/`.

```
~/.config/cosmic/com.system76.CosmicPanel/v1/entries          -> ["Panel", "Dock"]
~/.config/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_center
~/.config/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_wings
```

### Workspaces have no id, and there is one group per output

`ext_workspace_manager_v1` requires the compositor to send an `id` event; COSMIC does
not. So a workspace can only be addressed as (group + name + grid position). And because
workspaces are **grouped per output**, both monitors have workspaces called `"1"` and
`"2"` — on an ambiguous name the tools require an explicit `group` and error out rather
than guess which monitor you meant. Groups can only be numbered by the order the
compositor reports them: the `wl_output` proxy passed to `workspace_enter` carries no
connector name here.

### AT-SPI gives no widget tree

`at-spi2-registryd` is running, but the a11y bus carries only itself and WorkBuddy's
magpie — **no COSMIC application registers at all**. libcosmic's accesskit goes through
`cosmic_a11y_manager_v1`, not AT-SPI. So "which buttons does this applet have" is a dead
question on COSMIC, and widget-level debugging stays black-box: screenshot, click, read
logs.

### Window geometry: `zcosmic_toplevel_info_v1` (wired up 2026-10-17)

The standard protocols do not expose a window's screen position
(`ext_foreign_toplevel_list_v1` carries only identifier/title/app_id); position exists
only in COSMIC's private `zcosmic_toplevel_info_v1` (v2+). Measurements:

- **No vendored XML, no build.rs.** `cosmic-protocols 0.2` on crates.io ships the generated
  bindings and the XML it scans, so adding a dependency is the whole job —
  `cleanCargoSource` needs no change. The earlier plan ("next step: vendor the XML") is
  obsolete.
- **Geometry is in logical coordinates relative to its output.** Verified: at DP-2 scale
  1.25 a kitty at logical 1012x1104 at (1028,40) crops to 1265x1380 at (1285,50) — exact
  at the integer ratio. Converting to screenshot pixels multiplies by `region.scale` only,
  never by `wl_output.scale` (which always reports 2 and cannot be trusted).
- **The initial state comes from the compositor's frame-commit path.** After
  `get_cosmic_toplevel`, cosmic-comp only pushes states/geometry on the next rendered
  frame (`ToplevelInfoState::refresh` hangs off the commit path). On an idle desktop
  nothing arrives at all. The code therefore polls for at most 600 ms and stops as soon as
  the first geometry lands.
- **Geometry is the last reported position, not visibility.** With mpv fullscreen, kitty's
  geometry still sits at its tiled position, so cropping by it returns the mpv pixels
  covering that area. For real content, `window_action activate` first.
- **The states array works** (activated / maximized / minimized / fullscreen / sticky), and
  `window_action activate` really does move focus — which closes the "EIS clicks do not
  move keyboard focus" gap: click, then activate, then `type_text` lands in the window you
  just clicked.
- **Workspace membership exists, but only at v3.** `zcosmic_toplevel_handle_v1` v3 adds
  `ext_workspace_enter` / `ext_workspace_leave`, carrying the standard
  `ext_workspace_handle_v1` objects. Joined against `ext_workspace_manager_v1` in the same
  connection, they give the group, name and `active` flag that `list_windows` reports. The
  group's `output_enter` event resolves to the `wl_output` connector name, so a window's
  workspace can be tied to the same output index its geometry uses. Bound at v2 (or without
  the workspace manager) the fields are `null` and the reply says why, rather than
  pretending the window is on no workspace. Verified live: a window on eDP-1 reports
  `{"group":0,"name":"1","active":true,"outputs":["eDP-1"]}`.

## The EIS backend (verified)

`--eis-probe` / `--eis-click X Y` take a libei connection through the RemoteDesktop
portal. What the compositor reports:

```
eis: device type=Virtual name=Some("virtual keyboard")
eis:   interfaces pointer_absolute=false button=false keyboard=true
eis: device type=Virtual name=Some("virtual absolute pointer")
eis:   region x=384 y=1152 1280x720  scale=1.5  mapping_id=eDP-1
eis:   region x=0   y=0    2048x1152 scale=1.25 mapping_id=DP-2
eis:   interfaces pointer_absolute=true button=true keyboard=false
```

Key points:

- A region is the whole desktop in **logical coordinates** (a 2560x1440 panel at scale
  1.25 is 2048x1152). Multiple monitors form one continuous coordinate space;
  `--eis-click 1024 8` landed exactly on the panel clock and opened the calendar popup.
- **An EIS click does not move keyboard focus** (measured 2026-10-17): whichever window
  receives the click, keyboard focus stays put. The fix is `window_action
  {action:"activate"}` through zcosmic-toplevel-manager, then `type_text`. The other
  consequence: keys sent by `key` / `type_text` go to the **currently focused window**,
  which need not be the one just clicked.
- Authorization uses `PersistMode::ExplicitlyRevoked`; the restore token lives in
  `$XDG_STATE_HOME/cosmic-desktop-mcp/restore-token`, after which every connection is
  **silently approved**. The token can inject input — never commit it anywhere.
- A trap found while debugging: reis buffers requests locally, so **`connection.flush()`
  after every event-loop pass** is mandatory. Without it `ei_seat.bind` is never sent, and
  the symptom is "the seat arrived but no device ever does".
- Does the compositor restrict real input while an EIS session is open? Measured: no —
  clicking and capturing both keep working.

- **`scroll`'s `dy` is wheel detents, positive up** (matching the kernel's `REL_WHEEL`),
  negative down. `ei_scroll`'s y axis points down, so `src/input.rs` passes `-dy` — which
  means `dy=-1` at the bottom of a scrollable area looks like "nothing happened". Do not
  read that as an undelivered event.
- Scrolling was verified with a purpose-built window: `kitty -o font_size=26 sh -c 'seq 1
  300; exec sleep 900'`, numbered lines. Three detents up moved the viewport from 295–300
  to 280–300; fifteen detents down returned exactly to the bottom. The panel clock is an
  unreliable target — it may not respond to the wheel at all.
  New windows open on the **focused output** (COSMIC's current default, not a guarantee),
  so capture both outputs when hunting for one; to place a window on a specific output,
  click that output first.

The MCP tool layer switched to EIS on 2026-10-17: `src/input.rs` speaks EIS only, the
session is long-lived (`EiEvent` is not `Send`, so it owns a thread fed by a command
channel), the key table lives in `src/keymap.rs`, and `/dev/uinput` is gone entirely.

## Releasing

The version is written in three places — `Cargo.toml`, the `cosmic-desktop-mcp` entry in
`Cargo.lock`, and `version` in `flake.nix` — and a release tag has to match all of them.
`scripts/bump-version.sh` writes the three; `scripts/check-version.sh` proves they agree.
CI runs the check on every push, and `release.yml` runs it again against the tag.

```bash
scripts/bump-version.sh 0.2.1
jj describe -m 'release: v0.2.1'        # or git commit -am
jj bookmark set main -r @ && jj git push -b main
git tag -a v0.2.1 -m 'v0.2.1' && git push origin refs/tags/v0.2.1
```

Pushing the tag runs `release.yml`: it checks the tag against the version, evaluates and
builds the flake (which pushes the result to the cache), and publishes a GitHub release
with generated notes.

The release carries no binary on purpose. The package is a Nix wrapper around the Rust
binary, so a tarball of `result/bin/cosmic-desktop-mcp` would reference store paths that do
not exist on the machine that unpacks it. What ships is the tag, the cache entry and the
flake: `nix run github:mtul0729/cosmic-desktop-mcp`.
## Known limitations

1. **`type_text` always goes through the clipboard.** Synthesized keycodes reach the input
   method first: fcitx5 in Chinese mode committed `abc123` as Chinese candidates, and even
   a single `z` opens the candidate popup. So the keycode path is opt-in via
   `mode:"keys"`, accepts printable ASCII only, and is right only when no input method is
   active or real key events are actually wanted. The clipboard is provided by
   wl-clipboard (packaged into the PATH automatically, nothing to install). Verified:
   `clipboard_write` round-trips Chinese text intact and also offers the XWayland
   compatibility types `TEXT`/`STRING`/`UTF8_STRING`, so Chrome/Electron apps can paste.
   One property to keep in mind: a Wayland selection lives as long as its owning process,
   so the clipboard is gone once the MCP server exits.
2. **Clicks do not move keyboard focus.** After `click`, `type_text` still goes to the
   window that was focused **before** the click, not the one just clicked. There is a fix:
   `window_action {action:"activate"}` through zcosmic-toplevel-manager moves focus
   (verified), then `type_text`.
3. **Geometry is the last reported position, not visibility.** When a window is covered
   (fullscreen video) or on another workspace, cropping by its coordinates returns
   whatever pixels are actually in that area. `list_windows` now reports
   `on_active_workspace` (and the workspace group/name) so the second case is visible
   before you click, and `click_window` warns when the target is not on the active
   workspace. For the first case, `window_action activate` first. Stacking order is not
   exposed, so a hit test with several overlapping candidates says it is ambiguous instead
   of picking one.
4. **`key`'s key table assumes a US layout.** Events enter the compositor's input pipeline
   and it applies the layout, which cannot be queried; for non-US layouts, use the
   clipboard.

## Why hand-write the MCP protocol layer instead of using an SDK

The protocol layer in `src/mcp.rs` is hand-written, about 60 lines: read stdin line by
line, parse JSON-RPC, dispatch on `method`, write replies back line by line. It implements
`initialize` / `ping` / `tools/list` / `tools/call` and nothing else.

The reason is dependency weight: the official Rust SDK, `rmcp` (v3.5.1, 1.0 due 2026-03,
supporting spec 2026-07-28), brings its own tokio runtime abstraction and schemars, which
would make crane's `cargoArtifacts` hash noticeably heavier — while the tools are still
changing fast, so cache reuse would drop.

**The cost is real**, not "good enough anyway":

| What is missing | Consequence |
|---|---|
| `protocolVersion` pinned to `2024-11-05` | Newer clients negotiate down; no `server/discover`, long-running tasks or subscriptions |
| No progress notification | The client sees nothing while a screenshot is taken or a portal authorization is awaited |
| No cancellation | A dispatched call cannot be cancelled (the first EIS authorization has a 60 s timeout) |
| Hand-written schema | Maintained apart from the implementation; the `scroll` `dy` description once said the opposite of the code |

**When to switch to `rmcp`** — any one of these ends the saving:

1. Progress feedback is needed (batch screenshots, waiting on authorization)
2. Cancellation is needed
3. Several clients must be served at once
4. Streamable HTTP transport is needed
5. The tool count keeps growing and hand-written schemas stop paying

**Migration path**, not necessarily all at once:

1. Move tool schemas to derives over Rust structs (bring in `schemars` alone, not `rmcp`)
   and eliminate schema drift — that is what caused the `dy` incident.
2. When progress / long tasks / HTTP are genuinely needed, switch to `rmcp`'s `#[tool]` +
   `ServerHandler`; the `tool_*` function signatures inside `dispatch()` carry over almost
   unchanged.

## Install

```bash
nix build github:mtul0729/cosmic-desktop-mcp
```

The result is `result/bin/cosmic-desktop-mcp`.

### MCP client environment

This is a Wayland client, so its stdio server needs the desktop session environment.
MCP clients are not required to inherit their full environment when spawning a local
server. Codex deliberately starts stdio servers with a cleared environment and only
forwards variables named in `env_vars`.

For Codex, configure it with:

```toml
[mcp_servers.cosmic-desktop]
command = "cosmic-desktop-mcp"
env_vars = ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "XDG_SESSION_ID"]
```

`WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` are required. `XDG_SESSION_ID` lets
`screenshot` ask logind whether the session is locked; without it, `screen_locked`
is `null`. If you override `XDG_CONFIG_HOME` or `XDG_STATE_HOME`, forward those
too. Restart the Codex session after changing `env_vars`: already-running MCP
servers keep their original environment.

To wire it into WorkBuddy (`~/.workbuddy/mcp.json`):

```json
{
  "mcpServers": {
    "cosmic-desktop": {
      "command": "/path/to/cosmic-desktop-mcp"
    }
  }
}
```

Or declaratively, in a NixOS configuration:

```nix
{
  inputs.cosmic-desktop-mcp.url = "github:mtul0729/cosmic-desktop-mcp";
  # Note: do **not** add
  # inputs.cosmic-desktop-mcp.inputs.nixpkgs.follows = "nixpkgs" here.
  # The output is already pushed to cachix `mtul`, and the derivation is computed
  # against this package's own nixpkgs; following yours produces a different
  # derivation, misses the cache, and rebuilds every Rust dependency on each switch.
  # The price of not following is one extra nixpkgs evaluation in the store, with an
  # RPATH pointing at its own glibc — indistinguishable at runtime.

  outputs = { self, nixpkgs, cosmic-desktop-mcp, ... }: {
    # ...
    environment.systemPackages = [ cosmic-desktop-mcp.packages.${pkgs.system}.default ];
  };
}
```

## Self-check

The self-check has two layers, for two different users:

- **`--doctor` is a one-off human task**: it verifies the install and, along the way,
  dismisses the EIS portal authorization dialog. It really captures the screen and really
  opens an EIS session, so it has side effects and is not suitable for an agent to call at
  will.
- **The `env_info` tool is the agent's status query**: once the MCP server is wired up the
  agent may call it at any time, and it has no side effects — it uses `eis::peek` to read
  an existing session, never opens one, so it never raises an authorization dialog.

Run doctor once before wiring up MCP:

```bash
cosmic-desktop-mcp --doctor
```

It verifies four things in turn and names the step that failed. Real output from this
machine:

```
wayland: Ok("wayland-1")
output #0: 1920x1080 scale=2 name=eDP-1
output #1: 2560x1440 scale=2 name=DP-2
toplevels: 2
  WorkBuddy [yKlXztLTFkE6KoyiT4A5RQgQ9OxjLidx] WorkBuddy
  kitty [yMeUdgSO2ua7TSqYvAllKYApFGasyKEg] hx ~/S/p/cosmic-desktop-mcp
capture #0: 1920x1080 ok (8294400 bytes rgba)
capture #1: 2560x1440 ok (14745600 bytes rgba)
backend: eis (libei through the RemoteDesktop portal)
device "virtual keyboard" serial=2 absolute=false button=false keyboard=true pointer=false scroll=false
device "virtual pointer" serial=3 absolute=false button=true keyboard=false pointer=true scroll=true
device "virtual absolute pointer" serial=4 absolute=true button=true keyboard=false pointer=false scroll=true
region Some("eDP-1") logical x=384 y=1152 1280x720 scale=1.5 -> physical 1920x1080
region Some("DP-2") logical x=0 y=0 2048x1152 scale=1.25 -> physical 2560x1440
```

Everything from `backend:` down is the input backend describing itself: the three virtual
devices the compositor provides (keyboard / relative pointer / absolute pointer) and each
output's region. Those regions are what coordinate conversion is built on.

### The agent's status query: env_info

Inside an MCP session the agent calls `env_info` and gets JSON:

- `outputs`: for every display, the connector name (`DP-2`), physical size, and the EIS
  region matched to it (logical origin, logical size, real scale). Regions are matched to
  outputs **by name first** (`wl_output.name` == the region's `mapping_id`); only when
  both sides lack a name does it fall back to physical size — and only when that matches
  exactly one region. **Two identical monitors (same physical size) are therefore an
  error**, instead of silently routing output 1's clicks to output 0 as before.
- `input.connected`: whether an EIS session exists. When `false`, the notes say "the first
  `click` raises a portal authorization dialog that a human must accept" — which is exactly
  how the agent diagnoses "I clicked and nothing happened".
- `notes`: the coordinate convention (tools take screenshot pixels; the logical
  conversion happens inside).

If `env_info` reports `missing Wayland session environment`, the desktop is not
the problem: the MCP client omitted `WAYLAND_DISPLAY` or `XDG_RUNTIME_DIR`.
Use the `env_vars` configuration above and restart the client.

### When the status goes stale

- Authorization can be revoked in system settings; after that the next session raises a
  dialog again.
- Plugging in monitors, changing scale or rearranging outputs all change the regions. The
  output list is enumerated live every time (`input.rs` does not cache), so no server
  restart is needed after a topology change.
- While the screen is locked, capture is silently suspended (see NIX.md), which shows up
  as a screenshot timeout.

## Development

```bash
nix develop          # cargo / rustc / rustfmt / clippy
cargo check
cargo build
```

`skill/cosmic-desktop-ops/SKILL.md` is part of the deliverable, not commentary: a renamed
tool, a changed argument, or a newly discovered trap means editing it in the same commit.
`cleanCargoSource` drops it from the Rust build source, which is intended — it is
documentation, not compilation input.

Packaging details are in [NIX.md](NIX.md).

## Protocol references

- `ext-image-copy-capture` — https://wayland.app/protocols/ext-image-copy-capture-v1
- `ext-foreign-toplevel-list` — https://wayland.app/protocols/ext-foreign-toplevel-list-v1
- `ei` (libei) — https://libei.readthedocs.io/
- `input-event-codes` — key/button code table, hand-written in `src/keymap.rs`, covering
  only what is used

## License

MIT

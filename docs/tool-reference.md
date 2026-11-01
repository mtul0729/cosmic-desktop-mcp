# Tool reference

This page describes the tools exposed by `cosmic-desktop-mcp` over MCP. Successful calls
return structured JSON text. Pointer and window tools also return enough context to verify
what they requested.

Optional arguments use their documented default only when omitted or `null`. A present
value with the wrong type or range is an error, not a fallback to the default. Unknown
argument names are rejected, so a misspelled `output` cannot silently become output `0`.

## Conventions

### Window selectors

Tools with a `window` argument accept:

1. the exact protocol `identifier` from `list_windows` (recommended),
2. an exact `app_id`, or
3. an exact title, or a unique case-insensitive title substring.

If a name or title matches more than one window, the tool returns an ambiguity error rather
than picking one. Use the identifier for repeated terminal windows and similar cases.

### Side effects

- **Observation:** `env_info`, `list_windows`, `screenshot`, `screenshot_window`. These do
  not open the EIS input session.
- **Preparation:** `prepare_input`. This may show the RemoteDesktop authorization dialog.
- **Actions:** `window_action`, pointer, keyboard, workspace, applet and config tools. Input
  actions open EIS lazily on first use and may therefore show the same dialog.

The server emits MCP progress notifications every two seconds while waiting for
authorization when the client supplies `_meta.progressToken`. Timeout, denial and portal
failure are reported as `authorization_timeout`, `authorization_denied` and
`authorization_unavailable` respectively.

### Coordinates

- `screenshot` produces **output screenshot pixels**. Pass these to `click`, `move` and
  `scroll` with the same `output` index.
- `screenshot_window` produces **window image pixels**. Pass its complete `coordinates`
  object unchanged to `click_window`, `move_window` and `scroll_window`.
- **Logical coordinates** are internal. No tool accepts them directly.

Never add a window origin to a point and call `click`. The window tools validate the window
identity and screenshot freshness, then resolve all conversions internally.

## Status and preparation

### `env_info`

Reports the desktop topology and input state. No arguments and no side effects.

Returns the server and protocol version, each output's index, connector name and physical
size, plus EIS logical region and scale when an input session already exists. Before input
authorization, logical region and scale are `null`; that is expected.

Use this before the first click, after changing monitors or scaling, or when a click seems
to be ignored.

### `prepare_input`

Opens or reuses the EIS input session. No arguments.

On first use this may show the system RemoteDesktop authorization dialog and wait for a
human. The response reports whether the session was newly opened, its devices and each
output's EIS region.

Calling this is optional because input actions open the session lazily. It is useful when
you want to trigger and diagnose authorization before taking an action.

## Screenshots and windows

### `screenshot`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `output` | integer | `0` | Output index as reported by `env_info` |

Captures one output without opening an input session. Returns the PNG path, image size,
output metadata, and `screen_locked` (`null` when logind cannot answer). The image is at
the output's full screenshot resolution. It is the pixel space accepted by `click`, `move`
and `scroll` with the same `output`. PNGs are written to a private `0700` directory with
mode `0600`; screenshot tools fail rather than use a shared temporary directory.
When set, `XDG_RUNTIME_DIR` must be absolute and refer to an existing directory.

If `screen_locked` is `true`, the image is the lock screen or a blanked desktop, not the
user's session. Capture can also time out while locked.

### `list_windows`

Lists open windows. No arguments.

Each window includes:

- `identifier`, `title`, `app_id`
- `state`: any of `maximized`, `minimized`, `activated`, `fullscreen`, `sticky`
- `output`: index, connector name and size
- `logical`: the compositor's placement relative to its output
- `pixels`: the same rectangle in that output's screenshot pixels
- `workspaces` and `on_active_workspace`

`pixels` is `null` until an EIS session exists, because the real scale comes from the EIS
region. `workspaces` and `on_active_workspace` can also be `null` when COSMIC does not
expose membership; `null` means unknown, not "on no workspace".

Geometry is the last reported placement, not visibility. A covered window or a window on an
inactive workspace can still have geometry. Activate the target before relying on the
image and click position matching. The window pointer tools reject minimized windows and
windows whose `on_active_workspace` is `false`.

The first geometry report can take up to about 600 ms on an idle desktop. Re-list if a
window has no geometry yet.

### `screenshot_window`

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `window` | string | yes | Window selector |

Captures one window without opening the input session. The response includes:

- `path` and `image`
- `capture.source`: `window` for direct capture or `output_crop` for a composited crop
- `capture.coordinate_safe`
- `window`, `output`, `logical`, `pixels`, `workspaces`, `on_active_workspace`
- `coordinates`: pass this object unchanged to the window pointer tools; it records the
  output identity and size as well as the image, logical rect and crop origin

Direct capture is preferred. It can show the target even when another window covers it, but
COSMIC may omit server-side titlebars. If the direct frame does not cover the complete
logical window, the tool will not guess.

When an input session is already open, it may fall back to an output crop. If no session is
open and a safe direct frame is unavailable, it returns a structured error with
`status: "needs_input_session"`. Run `prepare_input`, have a human approve, then take a new
window screenshot. If a session is open but the output has no usable EIS region, the status
is `no_usable_geometry` and the next step is to inspect `env_info` instead of reopening the
session.

### `window_action`

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `window` | string | yes | Window selector |
| `action` | string | yes | `activate`, `close`, `maximize`, `unmaximize`, `minimize`, `unminimize`, `fullscreen` or `unfullscreen` |

Sends a request to the compositor. It does not wait for or guarantee the requested state.
The reply names the resolved window and its identifier, even when the selector was an
`app_id` or title. Re-run `list_windows` to confirm the result.

**Use `activate` before typing.** An EIS click does not move keyboard focus on COSMIC, so
without activation `type_text` and `key` go to the previously focused window.

## Pointer input

The pointer tools return the output and logical point they used, a hit test for the window
under the point, and lock state. Read `hit.window`, `hit.ambiguous`,
`on_active_workspace`, and `other_output_match` when an action appears not to work. EIS
does not acknowledge delivery, so action replies include `verified: false`; take another
screenshot to confirm the result.

### `click`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `x`, `y` | integer | required | Pixel coordinates from `screenshot` |
| `output` | integer | `0` | Output index for those coordinates |
| `button` | string | `left` | `left`, `right` or `middle` |
| `count` | integer | `1` | `1` for single-click, `2` for double-click |

The output is never inferred from `x`/`y`. Coordinates from one monitor's screenshot must
be sent with that monitor's output index. Use `click_window` for a point taken from
`screenshot_window`.

### `move`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `x`, `y` | integer | required | Pixel coordinates from `screenshot` |
| `output` | integer | `0` | Output index for those coordinates |

Moves the pointer without clicking. Useful for hover state and as a non-clicking probe.

### `scroll`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `x`, `y` | integer | required | Pixel coordinates from `screenshot` |
| `output` | integer | `0` | Output index for those coordinates |
| `dy` | integer | `0` | Vertical wheel detents; positive is up |
| `dx` | integer | `0` | Horizontal wheel detents; positive is right |

At least one of `dx` and `dy` must be non-zero.

Positive `dy` follows the kernel `REL_WHEEL` convention and reveals earlier content.
Negative `dy` scrolls down. Testing downward scroll at the bottom of a list proves nothing;
start near the top when checking whether scrolling works.

### `click_window`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `window` | string | required | Window selector |
| `x`, `y` | integer | required | Pixels from that window's screenshot |
| `coordinates` | object | required | The complete `coordinates` object from `screenshot_window` |
| `button` | string | `left` | `left`, `right` or `middle` |
| `count` | integer | `1` | `1` for single-click, `2` for double-click |

Validates that `coordinates` belongs to the selected window and that its output identity,
size, crop origin and logical rect are still current, then converts the image point to
output pixels and logical desktop coordinates. Do not reconstruct or edit the
`coordinates` object.

### `move_window`

Same arguments as `click_window`, without `button` and `count`. Moves the pointer to a
point in the window image.

### `scroll_window`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `window` | string | required | Window selector |
| `x`, `y` | integer | required | Pixels from that window's screenshot |
| `coordinates` | object | required | The complete `coordinates` object from `screenshot_window` |
| `dy` | integer | `0` | Vertical wheel detents; positive is up |
| `dx` | integer | `0` | Horizontal wheel detents; positive is right |

Scrolls at a point in the window image. At least one of `dx` and `dy` must be non-zero.
Uses the same freshness and coordinate validation as `click_window`.

## Keyboard

### `type_text`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `text` | string | required | Text to enter |
| `mode` | string | `paste` | `paste` uses clipboard + Ctrl+V; `keys` sends keycodes |

`paste` is the reliable mode and supports non-ASCII text. It overwrites the current
clipboard. The target must accept Ctrl+V.

`keys` synthesizes keycodes and accepts printable ASCII, spaces, tabs and newlines. It is
intended for cases where no input method is active or real key events are required. With
fcitx5 in Chinese mode, ASCII can be interpreted as pinyin and turned into candidates.

Text goes to the **currently focused** window. Activate the intended window first.
The reply includes `screen_locked`, `lock_evidence` and `verified: false`.

### `key`

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `combo` | string | yes | Key chord such as `ctrl+s`, `alt+tab` or `Return` |

Sends one chord to the currently focused window. The key table assumes a US layout; use
`type_text` for text on other layouts. The reply includes lock state and `verified: false`.

## Workspaces

### `workspaces`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `activate` | string | omitted | Workspace name (`"2"`) or grid position (`"2,0"`) |
| `group` | integer | omitted | Output workspace group, from the listing |

With no `activate`, lists workspace groups, names, grid positions and
active/urgent/hidden state. COSMIC has one workspace group per output, so groups commonly
share names such as `"1"` and `"2"`. When a name exists in several groups, pass `group`
explicitly; the tool will not guess.

## Applets and COSMIC config

### `applets`

Lists running panel applets from `/proc`: process name, PID, parent, memory and age. One
panel per output generally means one copy of each applet per output. No arguments.

### `restart_applet`

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Full applet name or suffix, such as `cosmic-applet-time` or `time` |

Blinks and rewrites the panel `plugins_*` config entry to make COSMIC respawn the applet.
This is a **panel-section refresh**, not a targeted single-process restart: other applets
in that section may also restart with new PIDs.

The original config text is restored and a private backup is kept in
`$XDG_STATE_HOME/cosmic-desktop-mcp/panel-backups/` (falling back to
`~/.local/state/...`); the response lists the exact backup paths. Do not use
`kill`/`SIGTERM`; COSMIC does not reliably respawn an applet that was killed.

Backups must succeed before any config is changed. Config writes are atomic, and an error
during panel-config discovery, reading, blanking or restoration aborts the operation rather
than skipping a section. An error during blanking or restoration also triggers a
best-effort restore through a drop guard.

Applet logs are intentionally not an MCP tool. The
[operations skill](../skill/cosmic-desktop-ops/SKILL.md) gives the `journalctl` recipe to use
with the process name reported by `applets`.

### `config_read`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `app` | string | omitted | COSMIC app ID |
| `key` | string | omitted | One setting name |

- No arguments: list known app IDs.
- `app` only: list all keys and values for that app.
- `app` and `key`: read one value and its file path.

COSMIC config lives under `~/.config/cosmic/<app-id>/v1/<key>`, one raw RON value per
file.

### `config_write`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `app` | string | required | COSMIC app ID |
| `key` | string | required | Setting name |
| `value` | string | omitted | Raw RON text; omit or pass `null` to delete |

Writes the value verbatim, so strings keep their quotes and enums keep their variant names.
The response includes the exact previous file text, including whitespace, so it can be
written back unchanged for rollback. `config_read` likewise returns the file text verbatim.
Most COSMIC consumers watch the config tree, but an applet that reads only at startup may
need `restart_applet`. A non-string, non-null `value` is rejected rather than treated as a
deletion.

## Related documentation

- [Quick start](../README.md)
- [Scope and design principles](design.md)
- [Engineering notes](engineering-notes.md)

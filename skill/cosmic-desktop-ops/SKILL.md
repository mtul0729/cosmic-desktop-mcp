---
name: cosmic-desktop-ops
description: Use when driving or debugging a COSMIC (Wayland) desktop through the cosmic-desktop-mcp tools — screenshots, clicking, typing, scrolling, window geometry and focus, panel applets, cosmic-config. Covers the three coordinate spaces (output screenshot pixels, window image pixels, logical) and which tool takes which, the focus and occlusion traps that make actions look like they failed, and the approaches already known to be dead on COSMIC (AT-SPI, SIGTERM-ing applets). Load before scripting a desktop task, or when a click or paste "did nothing".
agent_created: true
---

# Driving and debugging a COSMIC desktop

These tools are not general-purpose desktop automation. They implement one loop:
**look → locate → act → look again**, with every coordinate living in one of
three explicitly named spaces. For anything inside a window, the point comes
from `screenshot_window` and goes to `click_window`; no output index, no
arithmetic.

Protocol-level findings (how `zcosmic_toplevel_info_v1` pushes geometry, why
cosmic-panel does not supervise its applets) live in the `cosmic-desktop-mcp`
README. This file only covers *how to use the tools and what will bite you*.

## Step 0: check the tools are actually there

```
tools/list → screenshot / list_windows / click / applets / ...
```

If they are missing, say the machine has no cosmic-desktop-mcp wired up and give
the install path. **Do not fall back to ydotool, wtype, or xdotool.** Those go
through `/dev/uinput` or X11: on COSMIC they either cannot see an EIS region (so
every coordinate is wrong) or do nothing at all, which turns "tool not
installed" into a fake "this UI is unclickable" bug.

If the tools exist but `env_info` reports `missing Wayland session environment`,
fix the MCP client configuration. It must forward `WAYLAND_DISPLAY` and
`XDG_RUNTIME_DIR` (and preferably `XDG_SESSION_ID`), then restart the server.
This is an MCP launch-environment failure, not a compositor or tool bug.

## Step 1: `env_info` for the topology

Once at the start of a session, and again after plugging in a monitor or
changing a scale. It returns how many outputs exist, each one's name,
resolution and scale, and whether the EIS session is already authorized. The
`output` indices used everywhere else follow this order.

`env_info` opens no EIS session and shows no permission dialog, so it is free to
call.

## Coordinates: three spaces, and one rule

- **output screenshot pixels** — what `screenshot {output}` returns. `click` /
  `move` / `scroll` take these **and the matching `output` index**. The output
  defaults to 0 and is never inferred from x/y, so a point from another monitor's
  screenshot silently clicks the wrong screen.
- **window image pixels** — what `screenshot_window {window}` returns. Pass a
  point straight to `click_window` / `move_window` / `scroll_window`; they
  resolve the window's output and crop origin themselves.
- **logical** — the compositor's desktop coordinates. No tool takes them.

Never add a window origin to a point and call `click`. The window tools exist so
that arithmetic cannot go wrong. If a pointer reply contains
`other_output_match`, you used the wrong output's screenshot space — the point
is empty here but inside a window there.

Do not derive scale from `wl_output.scale`. COSMIC always reports 2 there
(integer protocol); the real ratio (1.25 / 1.5) only exists inside the EIS
region the tool reads, and `screenshot_window` reports it as `scale`.

**Clicking inside a window:**

```
screenshot_window {window: "kitty"}
→ { "image": {"width": 942, "height": 1008},
    "logical": {"x": 644, "y": 40, "width": 628, "height": 672},
    "pixels":  {"x": 966, "y": 60, "width": 942, "height": 1008},
    "origin":  {"x": 966, "y": 60},
    "output":  {"index": 1, "name": "eDP-1", "scale": 1.5},
    "on_active_workspace": true, ... }

click_window {window: "kitty", x: 468, y: 84}
```

Read the point off the image and pass it unchanged. `click_window` converts it
to output pixels and logical coordinates and says which ones it used.

## The loop

1. `screenshot {output}` → open the PNG with the Read tool and actually look at it;
2. pick a pixel coordinate (what you see on the image *is* the coordinate);
3. `click_window` inside a window (or `click` for the desktop), then `type_text` /
   `key` / `scroll_window`;
4. **take another screenshot to confirm.** Never assume an action landed: the
   compositor is free to ignore requests (see below).

For a read-only probe use `move` (hover) instead of `click`. `clipboard_read`
is worth checking before pasting anything.

## The focus trap (most common, most often mistaken for a broken tool)

**`click` does not move keyboard focus.** A button press injected through EIS
skips the compositor's focus negotiation, so

```
click → type_text
```

sends the text to whatever window was focused *before* the click. The correct
order is

```
window_action {window: "...", action: "activate"} → type_text
```

`activate` really does move focus (verified). Pure mouse work — pressing a
button, dragging a scrollbar — needs no activate. **Anything followed by
keystrokes does.**

## Windows: geometry, occlusion, and picking the right one

`list_windows` returns `identifier`, `title`, `app_id`, `output.name`, `state`
(activated, maximized, minimized, fullscreen, …), plus `logical` and `pixels`.

- **Address windows by `identifier`.** `app_id` and title are accepted only when
  they match exactly one window. Two kitteys and the call fails with an
  ambiguity error — deliberately, because silently picking the first one is how
  clicks land in the wrong place.
- **Geometry is the last reported position, not visibility.** With mpv
  fullscreen, kitty's geometry still sits at its tiled coordinates, so cropping
  by it returns the mpv pixels covering that area. **`activate` first, then
  capture.**
- **Workspace membership is reported.** `workspaces` lists the group, name and
  `active` flag; `on_active_workspace` is the short answer. When it is `false`,
  the window's geometry describes a workspace the output is not showing, so a
  click there hits whatever is visible instead — `click_window` warns about
  this, but check before trusting a screenshot. `null` means the compositor did
  not expose membership, not that the window is on nothing.
- **The first enumeration may take ~600 ms before geometry appears.** The
  compositor pushes the initial state from its frame-commit path, so an idle
  desktop waits for the next frame. That is by design, not a hang.
- `window_action` only **sends a request**; the compositor may ignore it. Re-run
  `list_windows` to see the resulting state. Actions: activate, close, maximize,
  unmaximize, minimize, unminimize, fullscreen, unfullscreen.
- New windows open on the **focused output** (current behaviour, not a
  guarantee), so check where focus is before concluding a window opened on the
  wrong screen.

## Lock screen (everything lies until you notice it)

When the session is locked the tools do not fail loudly — they mislead:

- `screenshot` now reports `screen_locked` (from logind) and `lock_evidence`;
  when capture is suspended it still times out with "timed out after 5s", and
  the error carries the lock state,
- `screenshot_window` also reports `screen_locked`, but the crop can still be a
  rectangle of **lock screen pixels**, because the window's geometry is
  unaffected by what covers it,
- every window looks frozen: clicks land nowhere useful, and activating does
  nothing.

If output suddenly looks like a greeter or stops making sense, **check the
clock and whether the session is locked before debugging anything else.** One
real incident cost half an hour this way. Ask the user to unlock; do not try
to automate your way out.

## Applets and the panel

`applets` lists every applet from `/proc`: name (taken from `argv[0]`, **not**
`/proc/<pid>/comm`, which is always `.cosmic-applets`), PID, parent, RSS, age.
Two panels means two copies of each applet.

**Never restart an applet with `kill` or SIGTERM.** cosmic-panel's source builds a
`ProcessManager` with exponential backoff, but in practice a SIGTERM leaves the
slot empty and the user's clock disappears. The only thing observed to work is
`restart_applet`, which blanks `plugins_<section>`, waits 1.2 s, then writes the
original text back.

Know what that costs: **it rebuilds the whole panel section**, so time,
workspaces and audio on that section all come back with new PIDs. Using it as a
single-applet restart quietly changes other things — tell the user afterwards.

`app_logs {name}` reads an applet's stderr from `journalctl -t <name>`, where
panics and warnings show up. That is the first place to look when an applet
renders wrongly.

## cosmic-config

Layout is `~/.config/cosmic/<app-id>/v1/<key>` — **one file per key**, containing
raw RON.

- `config_read` first; `config_write` returns the previous value, so **keep it in
  context** for a rollback. Reading once and discarding it defeats the point.
- The value is raw RON: strings keep their quotes, enums keep their variant
  names (`Some("...")`, `None`). Not JSON.
- Most consumers watch the config tree and react immediately; applets that only
  read at startup need `restart_applet` afterwards.
- To delete a key, omit `value` (or pass null).

## Dead ends (do not re-investigate)

- **AT-SPI has no widget tree here.** The a11y bus carries no COSMIC
  applications at all; libcosmic uses `cosmic_a11y_manager_v1`, not AT-SPI. Do
  not reach for pyatspi or accerciser to locate widgets — screenshot, click, and
  read logs instead.
- **Workspaces have no id.** Identity is (group, name, grid position), and the
  same workspace name exists on both monitors. On ambiguity, report it; never
  guess.
- **Synthesized keycodes go through the US layout** and through the input method
  first: under fcitx5 in Chinese mode, `abc123` turns into candidates. That is
  why `type_text` defaults to clipboard + ctrl+v; `mode: "keys"` is only for
  when no input method is active.
- **The clipboard lives with the MCP server process.** Exit the server and the
  selection is gone.
- **Scroll direction:** positive `dy` scrolls **up** (`input.rs` flips it). To
  test whether scrolling works, put the target at the *top* of a scrollable area
  and scroll down; testing downward scroll at the bottom of a list proves
  nothing.

## Recipes

| Symptom | Sequence |
|---|---|
| A widget will not accept a click | `window_action activate` → `screenshot_window` → pick (x,y) off that image → `click_window {window, x, y}` → screenshot again |
| Text lands in the wrong window | `window_action activate` → `type_text` |
| A window screenshot shows something else | It is occluded or on another workspace: activate, then `screenshot_window` |
| The click does nothing at all | Read the reply: `hit.window` (null = desktop), `on_active_workspace`, and `other_output_match` (wrong output's screenshot space). Use `click_window` for window images |
| An applet looks wrong | `applets` for the PID → `app_logs` → `config_read` the config → `restart_applet` if needed (and say the whole section was rebuilt) |
| Unsure about the desktop topology | `env_info` (no side effects). Do not guess |

## Anti-patterns

- Hardcoding absolute coordinates — breaks on any monitor or scale change.
- Adding a `screenshot_window` origin to a point and calling `click` — use
  `click_window` and pass the image point unchanged.
- `click` followed directly by `type_text`.
- Restarting applets with `kill`, or treating `restart_applet` as a targeted restart.
- Writing cosmic-config without keeping the previous value.
- Firing several clicks without verifying in between.
- Blaming the tool when coordinates do not match. Run `env_info` and
  `list_windows` first: a wrong coordinate is usually a wrong output index or an
  occluded window.

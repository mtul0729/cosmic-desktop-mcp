---
name: cosmic-desktop-ops
description: Use when driving or debugging a COSMIC (Wayland) desktop through the cosmic-desktop-mcp tools — screenshots, clicking, typing, scrolling, window geometry and focus, panel applets, cosmic-config. Covers the coordinate convention (screenshot pixels, per output), the focus and occlusion traps that make actions look like they failed, and the approaches already known to be dead on COSMIC (AT-SPI, SIGTERM-ing applets). Load before scripting a desktop task, or when a click or paste "did nothing".
agent_created: true
---

# Driving and debugging a COSMIC desktop

These tools are not general-purpose desktop automation. They implement one loop:
**look → locate → act → look again**, with every coordinate living in the
screenshot pixel space of one specific output.

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

## Step 1: `env_info` for the topology

Once at the start of a session, and again after plugging in a monitor or
changing a scale. It returns how many outputs exist, each one's name,
resolution and scale, and whether the EIS session is already authorized. The
`output` indices used everywhere else follow this order.

`env_info` opens no EIS session and shows no permission dialog, so it is free to
call.

## Coordinates: there is exactly one convention

- Every `x`/`y` is a **screenshot pixel**. Screenshots are not scaled, so **never
  multiply by 1.25 or 1.5** yourself.
- Coordinates **belong to one output**. `click {x, y, output: n}` only makes
  sense with the `n` the screenshot was taken from. Two monitors, two unrelated
  coordinate spaces; the wrong `n` clicks the same pixel on the other screen.
- Do not derive scale from `wl_output.scale`. COSMIC always reports 2 there
  (integer protocol); the real ratio (1.25 / 1.5) only exists inside the EIS
  region the tool reads.

**Clicking inside a window.** `screenshot_window` returns the answer directly:

```
.../window-<ts>.png
1265x1380 from output_index=1 (DP-2), window "kitty"
window origin in that output's screenshot pixels: x=1285 y=50
to click something at (dx,dy) inside this image use click(x=1285+dx, y=50+dy, output=1)
```

Copy that line. The returned origin is already in that output's pixel space, so
there is no arithmetic to do.

## The loop

1. `screenshot {output}` → open the PNG with the Read tool and actually look at it;
2. pick a pixel coordinate (what you see on the image *is* the coordinate);
3. `click` / `type_text` / `key` / `scroll`;
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
- **The first enumeration may take ~600 ms before geometry appears.** The
  compositor pushes the initial state from its frame-commit path, so an idle
  desktop waits for the next frame. That is by design, not a hang.
- `window_action` only **sends a request**; the compositor may ignore it. Re-run
  `list_windows` to see the resulting state. Actions: activate, close, maximize,
  unmaximize, minimize, unminimize, fullscreen, unfullscreen.
- New windows open on the **focused output** (current behaviour, not a
  guarantee), so check where focus is before concluding a window opened on the
  wrong screen.

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
| A widget will not accept a click | `screenshot` → pick pixel → `window_action activate` → `screenshot_window` for the origin → `click(origin+dx)` → screenshot again |
| Text lands in the wrong window | `window_action activate` → `type_text` |
| A window screenshot shows something else | It is occluded or on another workspace: activate, then `screenshot_window` |
| The click does nothing at all | Check the `output` index, check the window's `output.name` in `list_windows`, check the coordinate is inside the window |
| An applet looks wrong | `applets` for the PID → `app_logs` → `config_read` the config → `restart_applet` if needed (and say the whole section was rebuilt) |
| Unsure about the desktop topology | `env_info` (no side effects). Do not guess |

## Anti-patterns

- Hardcoding absolute coordinates — breaks on any monitor or scale change.
- `click` followed directly by `type_text`.
- Restarting applets with `kill`, or treating `restart_applet` as a targeted restart.
- Writing cosmic-config without keeping the previous value.
- Firing several clicks without verifying in between.
- Blaming the tool when coordinates do not match. Run `env_info` and
  `list_windows` first: a wrong coordinate is usually a wrong output index or an
  occluded window.

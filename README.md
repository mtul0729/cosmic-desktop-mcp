# cosmic-desktop-mcp

A small open-source utility that exposes native COSMIC desktop operations over MCP: take
screenshots, inspect windows and workspaces, move the pointer, click, scroll, and type. It
talks to COSMIC's native Wayland protocols directly; X11 tools such as `xdotool` and
`scrot` are not used.

It is useful for UI testing, desktop troubleshooting, panel-applet debugging, or a small
amount of supervised desktop automation.

> **Platform:** Linux + COSMIC on Wayland. Tested against COSMIC 1.9.0. The MCP package is
> published for `x86_64-linux` and `aarch64-linux`.

## Design

This is a small, focused utility rather than a general desktop automation platform. It is
not a shell replacement or a security boundary; the caller is expected to have normal access
to the machine, and MCP is only the interface used to expose the desktop operations.

The tool surface stays focused on COSMIC desktop state, coordinates and input. Workflows,
traps and ordinary shell recipes belong in the [operations skill](skill/cosmic-desktop-ops/SKILL.md).
See [Scope and design principles](docs/design.md) for the decisions behind the tool surface,
side effects, coordinate handling and error behaviour.

## What it can do

| Area | Capabilities |
|---|---|
| Look | Capture the whole screen or one window; list windows, geometry and workspace membership |
| Control | Move, click, double-click, scroll, type text, send key chords, switch workspaces |
| Windows | Activate, close, minimize, maximize and fullscreen a window |
| Clipboard | Read and write text without immediately pasting it |
| COSMIC debugging | List panel applets, refresh a panel section, read/write COSMIC config |

See the [tool reference](docs/tool-reference.md) for every tool and argument.

## Before you start

- A COSMIC Wayland session. XWayland-only approaches cannot see native COSMIC windows.
- The RemoteDesktop portal. COSMIC provides it; the first input action asks for permission.
- [Nix](https://nixos.org/) with flakes for the recommended install. A regular Rust build is
  also supported.

The Nix package bundles `wl-clipboard`, which the clipboard and paste tools use.

## Quick start

Run the self-check first:

```bash
nix run github:mtul0729/cosmic-desktop-mcp -- --doctor
```

This captures each output and opens an input session, so it may show the system's
RemoteDesktop authorization dialog. Approve it once; later sessions use a saved restore
token.

Install it:

```bash
nix profile install github:mtul0729/cosmic-desktop-mcp
cosmic-desktop-mcp --doctor
```

Or build a local binary:

```bash
nix build github:mtul0729/cosmic-desktop-mcp
./result/bin/cosmic-desktop-mcp --doctor
```

### Connect an MCP client

The server speaks MCP over stdio. Start it with no arguments:

```bash
cosmic-desktop-mcp
```

For Codex, add this to its configuration:

```toml
[mcp_servers.cosmic-desktop]
command = "cosmic-desktop-mcp"
env_vars = ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "XDG_SESSION_ID"]
```

For clients that use the common JSON configuration:

```json
{
  "mcpServers": {
    "cosmic-desktop": {
      "command": "cosmic-desktop-mcp"
    }
  }
}
```

Forwarding the Wayland environment variables is preferred. The server can discover a
single `wayland-*` socket in the runtime directory, but it deliberately refuses to guess
when several sockets exist. If you override `XDG_CONFIG_HOME` or `XDG_STATE_HOME`, forward
those too. Restart the MCP client after changing its server configuration.

### NixOS

Add the flake input:

```nix
{
  inputs.cosmic-desktop-mcp.url = "github:mtul0729/cosmic-desktop-mcp";
}
```

Then install it from a NixOS module (assuming `inputs` is passed through
`specialArgs`):

```nix
{ pkgs, inputs, ... }: {
  environment.systemPackages = [
    inputs.cosmic-desktop-mcp.packages.${pkgs.system}.default
  ];
}
```

Do not add
`inputs.cosmic-desktop-mcp.inputs.nixpkgs.follows = "nixpkgs"`. The published derivation
is built against the flake's own nixpkgs and is available from the `mtul` Cachix cache;
following your nixpkgs changes the derivation and forces a full rebuild. More packaging
details are in [NIX.md](NIX.md).

### Build with Cargo

```bash
nix develop
cargo build --release
./target/release/cosmic-desktop-mcp --doctor
```

For a Cargo build outside the development shell, install `wl-clipboard` and make sure
`wl-copy` / `wl-paste` are on `PATH`.

## First session

A safe first interaction looks like this:

1. Call `env_info`. It reports outputs and whether input is ready without opening an
   authorization dialog.
2. Call `screenshot` and open the returned PNG. Use the output index in the response.
3. Call `list_windows` to find a target by identifier, app ID or title.
4. Call `screenshot_window` for that target. Keep the returned `coordinates` object.
5. Pick a point from the window image and call `click_window`, passing `coordinates`
   unchanged.
6. Before typing, call `window_action` with `action: "activate"`. Then call `type_text`.
7. Take another screenshot to verify the result.

For whole-desktop points, use `click` / `move` / `scroll` with the pixel coordinates and the
**same output index** as the `screenshot`. For points inside a window, always use the
`*_window` variants.

The first input action may show a RemoteDesktop authorization dialog and wait for a human.
The server reports a structured `authorization_timeout`, `authorization_denied` or
`authorization_unavailable` error if that does not succeed.

## The one coordinate rule

There are three coordinate spaces:

| Space | Produced by | Accepted by |
|---|---|---|
| Output screenshot pixels | `screenshot` | `click`, `move`, `scroll` with the same `output` index |
| Window image pixels | `screenshot_window` | `click_window`, `move_window`, `scroll_window` with its `coordinates` object |
| Logical desktop coordinates | COSMIC's compositor | Internal use only; no MCP tool accepts these |

**Never add a window origin to a point and call `click`.** Use `click_window` and pass the
complete `coordinates` object from the screenshot that supplied the point. Mixing these
spaces can silently click a different monitor.

`click` defaults to output 0 and never infers the output from `x`/`y`. If a pointer response
contains `other_output_match`, the coordinates belong to another output's screenshot.

## Things that surprise people

- **Clicks do not move keyboard focus.** Activate the target window before sending keys:
  `window_action activate` → `type_text` / `key`.
- **Window geometry is placement, not visibility.** A window can be covered or on an
  inactive workspace. `capture.source` and `on_active_workspace` tell you which image you
  received; activate the window when the image and the click target must agree.
- **`type_text` uses the clipboard by default.** This is intentional: synthesized keycodes
  pass through the active input method and can turn into Chinese candidates. `mode: "keys"`
  is the opt-in ASCII-only alternative. Paste mode overwrites the current clipboard.
- **A locked screen suspends capture.** `screenshot` reports `screen_locked` when logind can
  answer; a capture may otherwise time out. Ask the user to unlock the session.
- **Do not kill a panel applet.** COSMIC does not reliably respawn it. Use
  `restart_applet`, which refreshes the whole panel section and keeps a config backup.

## Tool overview

| Group | Tools |
|---|---|
| Status | `env_info`, `prepare_input` |
| Observation | `screenshot`, `list_windows`, `screenshot_window` |
| Windows | `window_action` |
| Pointer | `click`, `move`, `scroll`, `click_window`, `move_window`, `scroll_window` |
| Keyboard and clipboard | `type_text`, `key`, `clipboard_write`, `clipboard_read` |
| Workspaces | `workspaces` |
| COSMIC debugging | `applets`, `restart_applet`, `config_read`, `config_write` |

Observation tools never open the input session. `prepare_input` and the action tools open it
lazily, so an authorization dialog can appear on first use.

## Troubleshooting

| Symptom | What to check |
|---|---|
| MCP server starts but reports no Wayland socket | Forward `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR` and `XDG_SESSION_ID`; restart the MCP client |
| First click waits or times out | Approve the RemoteDesktop dialog; inspect the structured authorization status |
| Click lands on the wrong monitor | Use the same output index as the screenshot, or switch to `click_window` |
| Click does nothing | Read `hit`, `on_active_workspace`, and `other_output_match` in the response |
| Text goes to the wrong window | Activate the intended window immediately before typing |
| `screenshot_window` returns `needs_input_session` | Run `prepare_input`, approve the dialog, then take a new window screenshot |
| Screenshot times out or looks like a greeter | Check whether the session is locked |
| A window screenshot shows different pixels from direct capture | Check `capture.source`; activate first if the image and click must match |

More recipes are in the [COSMIC desktop operations skill](skill/cosmic-desktop-ops/SKILL.md).

## Security and privacy

This server can observe and control your desktop. Only expose it to callers you trust.
The RemoteDesktop restore token is stored at
`$XDG_STATE_HOME/cosmic-desktop-mcp/restore-token` (with the usual state-directory
fallback). The containing directory is created with mode `0700` and the token with mode
`0600`; permission or storage failures are reported in the input-session detail. It can
authorize input injection, so treat it as a secret and never commit it.

MCP is an interface, not a sandbox. Restricting a client to this server does not make
desktop control safe or least-privilege.

`config_write` modifies COSMIC's configuration and `restart_applet` refreshes panel
processes. Both return enough information to roll back, but an agent should confirm
before making disruptive changes.

## Documentation

- [Tool reference](docs/tool-reference.md)
- [Scope and design principles](docs/design.md)
- [Engineering notes](docs/engineering-notes.md) — protocol research, measurements and known traps
- [Nix packaging notes](NIX.md)
- [Agent operations skill](skill/cosmic-desktop-ops/SKILL.md)

## Development

```bash
nix develop
cargo check
cargo test
cargo clippy
```

If you rename a tool or change its arguments, update the tool definition, the tool
reference, and `skill/cosmic-desktop-ops/SKILL.md` together. Ordinary shell recipes belong
in the skill; add an MCP tool only when the operation needs COSMIC-specific state or
semantics.

## License

MIT

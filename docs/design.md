# Scope and design principles

**English** | [简体中文](design.zh-CN.md)

`cosmic-desktop-mcp` is a small open-source utility that exposes a focused set of COSMIC
desktop operations over MCP. It is not a platform and does not try to provide a complete
desktop automation stack.

## Scope

- Take screenshots of COSMIC outputs and windows without X11 or GNOME-specific tools.
- Read window state, placement and workspace membership.
- Send pointer and keyboard input through COSMIC's EIS path.
- Keep coordinate conversion and the RemoteDesktop authorization flow in one implementation
  instead of repeating them in scripts or in every MCP client.

The caller is expected to have normal access to the machine, usually through a shell or
another host tool. MCP is only the interface used to expose these desktop operations. It is
not a shell replacement, a sandbox, or a permission boundary.

## Design principles

### 1. Use native mechanisms first

Use the protocols and mechanisms COSMIC actually implements. Do not add an X11,
`/dev/uinput`, GNOME Shell or `wlr-screencopy` fallback just to make an old Linux recipe
work. Protocol findings and failed approaches belong in the
[engineering notes](engineering-notes.md) so they are not rediscovered.

### 2. Expose desktop semantics, not shell commands

The MCP surface is for COSMIC state, geometry, capture and input. An ordinary command does
not become a desktop capability just because it can be wrapped in JSON. Keep command
recipes in the [operations skill](../skill/cosmic-desktop-ops/SKILL.md).

### 3. Make coordinate spaces explicit

Output screenshot pixels, window image pixels and logical desktop coordinates are different
spaces. Tools must not infer an output from `x`/`y`, and a window image must be passed back
with the exact `coordinates` object that produced it. When a conversion is ambiguous, return
an error instead of guessing.

### 4. Separate observation, preparation and action

Observation tools must not open an input session or raise a portal dialog. `prepare_input`
is the explicit preparation step. Input tools may open the session lazily, but they must
report that they are actions and include enough context to verify what happened.

### 5. Never quietly guess

Reject ambiguous window selectors, workspace names and output matches. Reject stale window
coordinates. Return structured errors when authorization is denied, unavailable or timed
out. A compositor request is not proof that the compositor honoured it, so callers must be
able to re-read the resulting state.

### 6. Make side effects visible and reversible

Pointer replies report the output used, logical point, hit test and lock state.
`config_write` returns the previous value. `restart_applet` restores the original config and
keeps a backup. Screenshots return their path and dimensions. An action that cannot be
verified should say so rather than present a success summary as certainty.

### 7. Keep authorization human-controlled

Input goes through the RemoteDesktop portal and its restore token. Lock-screen behaviour is
reported, not bypassed. The server does not try to manufacture consent or hide a blocked
capture. Access to the server is desktop control, not a sandboxed capability.

### 8. Keep the tool surface small

Every MCP tool costs schema text, documentation and maintenance. Add a tool only when it
needs COSMIC-specific state or semantics that the caller should not have to reconstruct.
Put sequencing, traps and ordinary shell recipes in the skill.

This is why `app_logs` was removed. It only validated a process name and ran:

```bash
journalctl --user -b --no-pager -n <lines> -t <name>
```

The operations skill now gives that recipe directly.

### 9. Measure against a real COSMIC session

The protocol path is not finished when it compiles. Verify capture, mixed-scale coordinates,
focus behaviour, locking and applet restart against a live session. Record measurements
that future changes could invalidate, and update the skill when a user-visible trap changes.

### 10. Prefer a small correct implementation

Do not add feature breadth that weakens the core guarantees. A smaller implementation with
clear failure modes is easier to trust and maintain than a broader one that silently chooses
the wrong monitor, window or workspace.

## Where new functionality belongs

A capability is a reasonable MCP tool when it needs COSMIC protocol state, desktop
geometry, input-session handling, or another COSMIC-specific structure.

If it is just an ordinary shell command wrapped in JSON, document it in the
[operations skill](../skill/cosmic-desktop-ops/SKILL.md). Keep sequencing and workarounds
there as well; tool descriptions should describe one operation, not the whole workflow.

## Non-goals

- A shell or a general command-execution API.
- A security boundary. Access to the server is access to desktop control.
- A generic Linux desktop automation tool; the implementation targets COSMIC Wayland.
- A screen-sharing, remote-login or system-management service.
- A wrapper around every shell utility that might help while debugging COSMIC.

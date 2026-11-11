# agentc

A headless Wayland compositor built for agents, with an MCP server and a human control panel in the same process.

Each window gets its own virtual screen, so an agent sees **one window = one coordinate space**, origin at the window's top-left corner. A screenshot is just the buffer the application submitted — no GPU, no EGL, no dmabuf, no DRM. The whole daemon is a static-ish 3.7 MB binary whose only system dependency is `libxkbcommon`.

```
┌─────────────────────────────────────────────────────────┐
│ agentc daemon                                           │
│                                                         │
│  Wayland compositor (smithay, calloop, single thread)   │
│    window 0 ── its own 1280×720 screen ─────────┐       │
│    window 1 ── its own 1280×720 screen ───────┐ │       │
│                                               │ │       │
│  HTTP :7788                                   │ │       │
│    POST /mcp   ← MCP (JSON-RPC 2.0)           │ │       │
│    GET  /      ← human panel (also an MCP     │ │       │
│                   client, X-Agentc-Actor)     │ │       │
│    GET  /ws    ← live frames + input events   │ │       │
└───────────────────────────────────────────────┼─┼───────┘
                                                │ │
                     agent (MCP client) ────────┘ │
                     human (browser) ────────────┘
```

## Why another compositor

Agentic computer use has a mature answer on X11 — `Xvfb` + `xdotool` — and nothing equivalent on Wayland. Wayland's security model means a client cannot learn where windows are, cannot enumerate them with geometry, and cannot inject input into someone else's surface. Every workaround (portal `RemoteDesktop`, `/dev/uinput`, `wlr-virtual-pointer`) is an outside-in hack that stops short of what a computer-use agent actually needs.

The geometry, the frame timing, and "who is in control" only exist inside the compositor. So this is a compositor that exists for that one job.

## Install

```bash
npx agentc-mcp daemon      # no build needed, binary comes from a per-platform npm package
```

or from source:

```bash
cargo build --release
# target/release/agentc
```

Requires `libxkbcommon` (only for keymap handling). Everything else is pure Rust.

## Run

| Command | What it does |
|---|---|
| `agentc daemon` | Long-running: compositor + MCP + panel |
| `agentc mcp` | stdio MCP relay; **starts the daemon if it is not running** |
| `agentc status` | Print the daemon URL |
| `agentc stop` | Stop the daemon |

The daemon writes its URL to `$XDG_RUNTIME_DIR/agentc.url` and listens on `127.0.0.1:7788` (next free port if taken).

The daemon runs under `setsid`, so when the MCP client exits and the agent session ends, the desktop and its applications are still there next time.

## npm packaging

`npx` works because the wrapper package carries no binary: `agentc-mcp` depends on
`agentc-mcp-linux-x64` / `agentc-mcp-linux-arm64` as `optionalDependencies`, so npm downloads only
the one that matches the machine (`esbuild`/`swc` and friends use the same trick).

```
npm/package.json                       wrapper, "bin": agentc.js
npm/bin/agentc.js                      picks the platform package, execs the real binary
npm/platform/linux-x64/package.json    os/cpu constrained, ships bin/agentc
scripts/pack-npm.sh                    builds + stages both packages
```

`./scripts/pack-npm.sh` then `npm publish` in `npm/` and `npm/platform/linux-x64/`.
For the whole platform matrix plus GitHub Releases, [`cargo-dist`](https://opensource.axo.dev/cargo-dist/)
does the same thing from one config file.

## Connect an agent

stdio, via the relay (recommended — it starts the daemon for you):

```jsonc
// opencode.json — from npm
{ "mcp": { "agentc": { "type": "local", "command": ["npx", "-y", "agentc-mcp", "mcp"] } } }

// or a local build
{ "mcp": { "agentc": { "type": "local", "command": ["/abs/path/agentc", "mcp"] } } }
```

```bash
claude mcp add agentc /abs/path/agentc mcp
# or: claude mcp add agentc npx -y agentc-mcp mcp
```

Or connect straight to HTTP if the daemon is already up:

```bash
claude mcp add --transport http agentc http://127.0.0.1:7788/mcp
```

## Tools

Coordinates are always window-local, `(0,0)` is the window's top-left.

| Tool | Notes |
|---|---|
| `screenshot {window?}` | PNG + `window id, WxH, revision N` |
| `windows` | id / app_id / title / size / mode / revision / dialog |
| `focus {window}` | keyboard focus |
| `click {x,y,button?,duration_ms?}` | `duration_ms` = how long the press lasts (default 60) |
| `double_click {x,y,interval_ms?}` | gap between presses, default 160ms |
| `move {x,y}` | hover |
| `drag {x1,y1,x2,y2,hold_ms?,steps?}` | real drag: hold, interpolate, release |
| `scroll {x,y,dy}` | |
| `type {text}` | any Unicode (CJK/emoji go via clipboard); trailing `\n` becomes Enter |
| `key {combo}` | `CTRL+C`, `ENTER`, `ALT+TAB`; modifiers are genuinely held |
| `wait_for_change {timeout_ms?}` | blocks until the screen changes, returns the new frame |
| `wait_for_idle {quiet_ms?,grace_ms?}` | blocks until the screen settles — use instead of sleeping |
| `wait_for_window {timeout_ms?,after_id?}` | blocks until a new window appears |
| `set_size {width,height}` | resize the virtual screen; invalidates old coordinates |
| `set_mode {mode}` | `windowed` / `maximized` (default) / `fullscreen` |
| `launch {argv,wait_ms?}` | spawns and waits for the window |
| `wait {ms}` | plain sleep |

Human-only tools, hidden from the agent: `status`, `pause`, `resume`.

## Human in the loop

Open `http://127.0.0.1:7788/` in a browser. It is a live remote desktop, not a status page:

- live frames pushed over a WebSocket, **only when the picture changes** (static screen ≈ 0% CPU)
- real input: pointer, wheel, keyboard by physical key, paste/IME for CJK
- `Pause` freezes the agent; every agent action then returns `PAUSED` and the model is told to stop and wait
- panel requests are marked `X-Agentc-Actor: human`, so they bypass the pause gate

The panel is just another MCP client. There is one API surface, and no privileged back channel.

## Design notes

**No renderer.** The compositor never composites for a display. A screenshot copies the client's own `wl_shm` buffer. There is no EGL, no GBM, no DRM, no scene graph, no damage ring.

**One virtual screen per window.** Each toplevel is configured to the same size, so the model sees a uniform `1280×720` image for every window and coordinates transfer across apps. `set_size` changes it and bumps a `revision`; acting on a stale revision is rejected rather than silently clicking the wrong place.

**Frame-accurate waiting.** `wait_for_idle` is built on a 16×16 block hash diff, not on sleeps: it returns when the screen actually stops changing. Two knobs matter — `min_area` (default 0.2% of the window) ignores blinking cursors and spinners, and `grace_ms` (default 200ms) prevents declaring "settled" before the UI has even started to react.

**Never block the event loop.** Every action that needs pacing (click hold, double-click gap, drag interpolation, typing, clipboard paste) is a tick-driven state machine. Nothing sleeps inside the compositor thread, so a human at the panel never queues behind an agent action.

**New windows are announced.** Dialogs often appear on their own (crash recovery, update prompts), so window creation is reported both as an MCP `notifications/message` push and attached to the next tool reply.

## Measured

On a 1280×720 animating window, release build:

| | |
|---|---|
| live stream | 30 fps, 96 KB/frame, 1.3 ms PNG encode |
| typing / cursor updates | 0–1 KB per frame |
| idle CPU | ~0.8% |

The stream encodes only the changed rectangle and switches to RGB (the canvas is always opaque), which is 4.6× faster than re-encoding whole RGBA frames.

## Limitations

- **Linux only.** It is a Wayland compositor; there is no macOS or Windows story.
- **No dmabuf.** Applications must be able to fall back to `wl_shm`. Mesa does this automatically, so Firefox, Chromium and GL clients do work, but everything is software-rendered.
- **X11 applications** need an external bridge such as [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite). None is bundled.
- **GTK3 menus** do not open under this compositor (they never create an `xdg_popup`); Chromium and GTK4 menus do.
- **Text input** is not implemented. `type` with non-ASCII goes through clipboard paste, which means terminals need `Ctrl+Shift+V` (handled automatically by app id).
- Experimental. The control protocol is internal and versioned only by "rebuild both halves".

## License

MIT.

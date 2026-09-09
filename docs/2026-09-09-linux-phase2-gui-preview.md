# Linux Phase 2 (GUI preview): M1 graphics and automation spike

Date: 2026-09-09

Source task: `9673bede` (Linux desktop support, Phase 2 — "GUI preview").
Plan gate: the approved plan front-loads M1 as a spike whose evidence decides
whether the remaining five milestones stay valid. **M1's exit gate is met on
the substance and short on one item.** The real Tauri app builds, runs through
`kd`, paints the full Kanna UI under WebKitGTK, answers W3C WebDriver
(session, evaluate, actions, screenshot), and opens a live PTY terminal that
renders with either xterm renderer. What is *not* proven here is two
simultaneous isolated instances, and no measurement below describes an
accelerated GPU path — this VM has none.

Everything measured here is from the Phase 0/1 VM: Ubuntu 26.04.1 aarch64 under
UTM/QEMU on the Mac Studio, GNOME Shell 50.1 (mutter 50.1), WebKitGTK 2.52.6,
GTK 3.24.52, Rust 1.93.1, Node 24.15.0, pnpm 11.0.8.

## 1. What the preview instance is

An isolated, desktop-owned Kanna instance, per the manual gate's decision — not
a GUI attached to Phase 1's `kanna-worker`. It is a separate checkout of this
task's committed tree at `~/kanna-p2` on the VM, started only through
`./kd dev up`, with `KANNA_WORKTREE=1` so `kd` derives its own database
(`kanna-wt-kanna-p2.db`), daemon directory, transfer root and tmux identity, and
with task-specific ports (dev 1520, WebDriver 4545, server 48220, transfer 4555,
mobile 8181, relay 9180). The desktop still spawns and authorizes its own
daemon and server; no trust check was changed and no daemon directory is shared
with the worker.

## 2. Infrastructure the plan asked the operator for

### Disk — solved, no operator action needed

The Phase 1 checkout's Rust build directory was reclaimed after its task merged
(PR #1370) and closed. Source and the owner's other data were untouched.

| | Free on `/` |
| --- | --- |
| Before (`~/kanna/.build` = 33 GB present) | **7.9 GiB** |
| After removing `~/kanna/.build` | **41 GiB** |
| After the Phase 2 checkout, its `node_modules`, six sidecars and a full debug `kanna-desktop` | **34 GiB** |

`~/.cargo/registry` (431 MB), `~/.cache/bazel` (209 MB) and `~/.cache/bazelisk`
(122 MB) were left alone — they are shared caches, and reclaiming them buys
little against a rebuild cost.

### Graphical session — solved without the operator, but not the whole gate

`loginctl list-sessions` still shows no graphical *user* session: the only
seat-0 session is `gdm-launch-environment` (class `greeter`, type `wayland`).
Every `jeremy` session is a remote SSH tty.

`Xvfb` is not installed and installing it needs the password, but it is not
needed. GNOME Shell 50.1's own headless mode is a **real** GNOME/Wayland
display server, needs no sudo, and is closer to the approved baseline than Xvfb
would have been:

```
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
  gnome-shell --headless --virtual-monitor 1600x1000 --wayland-display kanna-p2
```

That yields `WAYLAND_DISPLAY=kanna-p2` plus an Xwayland display, and the app
connects to it. **What it does not give is the human gate.** The owner still has
to log in once at the VM console for the on-VM visual and interaction
verification the plan and AGENTS.md require; the headless session is for
unattended functional coverage only, exactly as the approved baseline scopes it.

### Phone-reachable network — not solved, operator decision

The VM is on UTM's shared/NAT network (`192.168.64.0/24` on `bridge100`),
reachable from the Mac Studio and nothing else. A phone on the house Wi-Fi
cannot route to `192.168.64.2`, and mDNS/Bonjour discovery does not cross that
boundary either. Physical-LAN pairing acceptance (M5) therefore needs either a
UTM **bridged** network adapter putting the VM on the same L2 segment as the
phone, or an explicit port forward plus a manual address entry — which would
prove the transport but not discovery. No network change was made.

## 3. Findings

### 3.1 The desktop window never reached the display through tmux

`kd`'s tmux layer forwarded exactly two environment variables into a window
(`-e`), the auto-sign-in pair; everything else rode on the environment of
whichever client first created the tmux *server*. On macOS that is invisible,
because the window system needs no environment. On Linux it means the desktop
window may be started with no `WAYLAND_DISPLAY`, and — worse — that a
`kd dev restart desktop` respawn silently drops whatever it did have.

`tools/kd/src/runtime/tmux.ts` now names the Linux session variables explicitly
(`WAYLAND_DISPLAY`, `DISPLAY`, `XAUTHORITY`, `XDG_RUNTIME_DIR`,
`XDG_SESSION_TYPE`, `XDG_CURRENT_DESKTOP`, `DBUS_SESSION_BUS_ADDRESS`,
`GDK_BACKEND`), Linux only; macOS keeps the two-key list byte for byte.
Verified on the VM by reading `/proc/<pane_pid>/environ` after both a fresh
`kd dev up` and a `kd dev restart desktop`.

### 3.2 WebKitGTK's default renderer hangs the app with no window and no error

This is the finding that cost the most time and is the one most likely to hit
anyone else.

The VM's user is not in the `render` or `video` groups, so `/dev/dri/card0` and
`/dev/dri/renderD128` are `EACCES`. Under that condition WebKitGTK's default
DMA-BUF renderer **never starts a WebProcess**: `WebKitNetworkProcess` appears,
`WebKitWebProcess` does not, and the UI process blocks in
`unix_wait_for_peer` — a `kanna-desktop` that is alive, has zero windows, and
prints nothing beyond three `libEGL warning:` lines and a Vulkan
`VK_ERROR_INCOMPATIBLE_DRIVER` that are all *also* printed on a healthy run.
`tauri-plugin-webdriver` answers `/status` normally and then refuses every
session with `no such window`, which reads like a WebDriver problem and is not.

`WEBKIT_DISABLE_DMABUF_RENDERER=1` starts the WebProcess. Rather than set it
unconditionally on Linux — which would downgrade the renderer on a machine
whose GPU works, and quietly invalidate exactly the graphics evidence this
phase exists to collect — `kd` now probes for the capability:
`linuxDesktopWebkitEnv` in `tools/kd/src/runtime/dev-plan.ts` opens
`/dev/dri/renderD*` for read/write and only sets the variable when none can be
opened. An explicit value in the environment always wins. The variable is in
the tmux forwarding list from §3.1, because a respawn that lost it would start
an app with no window.

**This is a workaround for a missing capability, not the intended steady
state.** The right fix is GPU access: adding the VM user to `render` and
`video` needs the password, so it is an owner action. Until then, no rendering
measurement from this VM describes an accelerated path.

### 3.3 The app builds, runs, and paints

- All six sidecars build for `aarch64-unknown-linux-gnu` and stage into
  `apps/desktop/src-tauri/binaries/`.
- `kanna-desktop` compiles and links against WebKitGTK 2.52.6 / GTK 3.24.52 in
  8m20s (debug, cold, 8 vCPU).
- `./kd dev up` brings up Vite on 1520, runs the binary, and the WebDriver
  plugin listens on 4545.
- The window is 1200×724 at `devicePixelRatio` 1, titled `Kanna`, on
  `http://localhost:1520/`, and renders the whole application: sidebar, empty
  state, agent-setup panel (it correctly detects OpenCode v1.18.29 as the only
  installed provider), search field, status bar, and the startup keyboard
  shortcuts modal. Fonts, spacing and the `<kbd>` chips are all correct.
  Screenshots `02-app-startup.png` and `04-terminal-dom.png`.
- `WEBGL_debug_renderer_info` reports `Apple Inc. / Apple GPU`. That is
  WebKit's spoof, not the machine: **the renderer strings cannot be used to
  identify the real GL stack on WebKitGTK**, so any renderer claim has to come
  from outside the page.

#### The hang that was not WebKit's fault

Before the app painted, it spent a long time alive with no window, and the
diagnosis is worth writing down because the symptom is indistinguishable from
§3.2 and the cause is unrelated. `strace` showed the main thread stopped at:

```
connect(36<UNIX-STREAM>, {sa_family=AF_UNIX, sun_path=@"/tmp/.X11-unix/X1"} <unfinished ...>
```

`mutter --headless` keeps `/tmp/.X11-unix/X1` for its own *managed services*
and never accepts an ordinary client, while the public `:0` belongs to the GDM
greeter and refuses authorization. A `DISPLAY` pointing at either one hangs GDK
at `connect()` forever, on Wayland, before any window exists — and
`GDK_BACKEND=x11` hangs the same way. The preview session therefore sets
`GDK_BACKEND=wayland` and no `DISPLAY` at all. That is a property of this
headless setup, not of the product, which is why it lives in the environment
file rather than in `kd`.

### 3.3.1 Terminals work, and the shell policy is doing its job

Driving `⇧⌘J` through WebDriver actions opens a *Repo shell* tab with a live
prompt:

```
jeremy@jeremy-QEMU-Virtual-Machine:~$
```

That prompt is `bash`, and it is the whole point of §3.6: this machine has no
`/bin/zsh`, and before the shell policy reached the desktop the frontend would
have asked for one. The PTY, the daemon and the terminal stream all work.

### 3.3.2 Both xterm renderers work under WebKitGTK — and dev builds never used one

`webgl` and `webgl2` contexts are both available in the app's webview.

With the DOM renderer, the terminal reports `{renderer: "dom", reason:
"requested"}`, zero canvases, and its painted rows are readable from the DOM.
With `?kannaTerminalRenderer=webgl`, the same terminal reports `{renderer:
"webgl"}`, three canvases (896×630 for text and selection, 32×23 for the
cursor), and **no DOM rows at all** — and the WebDriver screenshot still shows
the painted text (`05-terminal-webgl-typed.png`). So on WebKitGTK a WebKit
snapshot does capture WebGL canvas content; the macOS behaviour that motivated
disabling WebGL under E2E does not reproduce here. That is a Linux observation,
not a reason to change the macOS default.

The surprise is how wide the old condition was. `window.__KANNA_E2E__` is
installed by `if (import.meta.env.DEV)` in `main.ts`, so the old
`if (!window.__KANNA_E2E__)` guard disabled the WebGL terminal renderer in
**every dev build**, not only under E2E — `./kd dev up` has never run the
renderer it ships. The opt-in from §3.5 is what makes that measurable at all.

### 3.3.3 Confirmed for M4: the shortcut hints are macOS glyphs on Linux

The startup modal renders every binding as `⌘`, `⇧`, `⌥`, and the status bar
says "Use ⌘ / to see available commands" — on a machine with no Command key.
The bindings dispatch on `metaKey`, which is why WebDriver can trigger them and
a person cannot. This is exactly M4's scope and is now reproduced rather than
predicted (`02-app-startup.png`).

### 3.3.4 Still open

- **Two simultaneous isolated instances** were not exercised. It needs a second
  checkout with its own Cargo build directory, which is a full second Rust
  build; it belongs with M6's harness work rather than with this spike.
- A page reload against a repo-less *Repo shell* surfaces `Failed to reconnect
  to existing session: shell cwd is not readable:` with an empty path. Recorded,
  not chased: it is a reconnect path with no repo selected, and nothing
  indicates it is Linux-specific.

### 3.3.5 Instance isolation and the loopback boundary, sampled on Linux

Two things the plan wanted verified before anything else is built on them, and
both hold on the real Linux listener rather than in a harness.

**The preview instance owns its own everything.** `kanna-desktop` (pid 4791)
spawns `kanna-daemon` and `kanna-server` as its own direct children from the
instance's `.build/`, against `kanna-wt-kanna-p2.db`, with its daemon directory
inside the checkout and its server state under
`~/.local/share/build.kanna/Kanna/servers/kanna-wt-kanna-p2/`. Its listeners are
the task's own ports (server 48220, transfer 4555, dev 1520, WebDriver 4545).
Nothing is shared with Phase 1's worker, and no trust check was touched.

`task-events.token` — the local control credential — is `0600`. `server.toml`
next to it is `0664` and contains `desktop_secret`; that is the pre-existing
cross-platform credential-storage follow-up the plan already named, and it is
confirmed to reproduce on Linux rather than being a Linux-only defect.

**The loopback-authority model behaves.** Against the real listener, with no
mock in the path:

| request to `/v1/tasks` | result |
| --- | --- |
| local process, no browser headers | admitted (405, wrong method — the route was reached) |
| `Origin: http://evil.example`, no credential | **403**, "browser requests must present this desktop's local control credential or a paired device secret" |
| `Sec-Fetch-Site: cross-site`, no credential | **403**, same |
| `Host: attacker.example` (DNS rebinding shape) | **403** |
| `Origin` + `Authorization: Bearer <local control token>` | admitted (405) |

This is a sample, not M5: it does not touch WebSocket first-frame auth, `no-cors`
mutation, pairing, or the real WebKitGTK request shapes. It does establish that
`lan_trust.rs` classifies correctly on Linux, which is what M5 builds on.

### 3.3.6 M4: the Linux keymap, and what it costs

§3.3.3 reproduced the defect; this is the fix. It is a keymap, not a glyph
substitution, because the obvious substitution is the dangerous one.

Mapping ⌘ to Ctrl would have handed the app every `Ctrl+<letter>` a terminal
owns: `Ctrl+C` (SIGINT), `Ctrl+D` (EOF), `Ctrl+Z`, `Ctrl+W`, and the rest of
readline — in an app whose main content *is* agent terminals. So:

| macOS | Linux | why |
| --- | --- | --- |
| `⌘X` | `Ctrl+Shift+X` | the GNOME Terminal convention, for this exact reason |
| `⇧⌘X` | `Ctrl+Alt+X` | its `Ctrl+Shift` form is taken by the line above |
| `⌃-`, `⌃⇧-` | unchanged | punctuation, not readline |
| `⌥⌘↑/↓` (task nav) | `Alt+↑/↓` | `Ctrl+Alt+Arrow` is GNOME's workspace switcher |
| `⇧⌘↑/↓` (repo nav) | `Ctrl+Shift+↑/↓` | same reason; the ⌘ tier never used arrows |
| `⇧⌘⌫` | `Ctrl+Shift+Backspace` | `Ctrl+Alt+Backspace` is the X-server-zap chord |
| `⌥⌘P` | `Ctrl+Alt+Shift+P` | `Ctrl+Alt+P` belongs to the command palette |
| terminal copy/paste `⌘C`/`⌘V` | `Ctrl+Shift+C`/`Ctrl+Shift+V` | plain `Ctrl+C` is SIGINT |

`shortcutPlatform.ts` holds the mapping and its named exceptions, and
`shortcutPlatform.test.ts` enforces the rules rather than the table: no Linux
binding may be a plain `Ctrl+<letter>`, no two may share a chord, and none may
sit on a GNOME-reserved arrow chord. Two of those tests failed on their first
run — repo navigation was on the workspace switcher — which is the point of
writing them as rules.

**The native GTK menu was the more dangerous half.** `CmdOrControl+W` on the
Close item resolves to `Ctrl+W` on Linux, and a GTK accelerator wins *before*
the keystroke reaches the webview, so no JavaScript can give it back. The
predefined Edit items are worse: they install `Ctrl+C`/`Ctrl+X`/`Ctrl+V`/`Ctrl+A`,
which would have made `Ctrl+C` open a menu instead of interrupting whatever the
agent is running. `menu_accelerators.rs` moves the Linux menu to the shifted
forms and drops the Edit accelerators entirely there; the webview already
provides editing in text fields. macOS is pinned unchanged by its own test.

Hints and dispatch now come from one place, so the shortcuts modal, the command
palette, the sidebar and main-panel empty states, and the task-search
placeholder all say what a Linux keyboard can actually press
(`06-linux-shortcuts.png`). Two rendering details that only a real run exposes:
the hint for `["_", "-"]` read `Ctrl+Shift+_`, a key nobody has, and the search
placeholder had `⌘F` baked into three locale files.

Clipboard **image** paste also worked on Linux for the first time.
`read_clipboard_image_png` returned `Ok(None)` unconditionally off macOS, and
`arboard`/`image` were macOS-only dependencies. Linux now shares the same
implementation: `arboard` links an X11 backend, so nothing shells out to
`xclip` or `wl-paste`, and on GNOME that is also the working path — Xwayland
bridges the selection, while the wlroots protocol `arboard`'s Wayland backend
needs is one mutter does not implement.

**Not done in M4**, and not claimed: IME and dead-key composition, native file
drops with spaces and Unicode, file dialogs, external file/URL opening, and
window minimize/fullscreen/focus-return. Those need the console session, and
interaction feel needs the human gate regardless.

### 3.4 Composited-window screenshots are not available unattended on this VM

GNOME 45+ refuses `org.gnome.Shell.Screenshot` to unsandboxed callers
(`AccessDenied: Screenshot is not allowed`), and the Screenshot portal wants a
click. Mutter's `ScreenCast` interface is allowed and works as far as creating a
session and publishing a PipeWire node (`gnome-shell`,
`Stream/Output/Video`) — but **no frames ever arrive**: `pipewiresrc`
negotiates, reaches `paused`, and times out with zero buffers, against both the
Python/GStreamer helper and a bare `gst-launch-1.0`. The most likely cause is
mutter's headless *surfaceless* renderer ("Created surfaceless renderer without
GPU"), i.e. the same missing `/dev/dri` access as §3.2.

Running the app on Xwayland instead is not an escape: mutter's headless mode
publishes `:0` under GDM's authority (`Authorization required`) and keeps its
own X socket for managed services, so `GDK_BACKEND=x11` simply hangs the app on
a display it cannot open.

So the screenshot paths available to an unattended Linux lane are:

1. `tauri-plugin-webdriver`'s Linux executor, which is a real WebKitGTK
   implementation — `evaluate_js` through JavaScriptCore, `take_screenshot` and
   `take_element_screenshot` through `webkit2gtk` `SnapshotOptions` /
   `SnapshotRegion`, plus `print_page` and `execute_async_script`. This captures
   what WebKit rendered, which is more than a DOM serialization and less than
   the compositor's output.
2. The composited window's actual pixels: **not obtainable here**, and therefore
   part of the owner's on-VM human gate rather than the automated lane.

### 3.5 The E2E terminal renderer could not describe production

`terminalView.ts` disabled the xterm WebGL addon whenever `window.__KANNA_E2E__`
existed. That default is right — WKWebView reports the WebGL-backed logical
buffer while capturing a blank native surface with a second desktop window open,
so screenshots have to stay tied to xterm's painted DOM rows — but it also meant
no E2E run could say anything about the renderer production actually uses, which
is precisely what this phase has to measure.

`apps/desktop/src/composables/terminalRenderer.ts` now separates the request
from the outcome: production always asks for WebGL, E2E still defaults to DOM,
and a rendering-specific run opts back in with
`window.__KANNA_E2E_TERMINAL_RENDERER__ = "webgl"`. The terminal records what it
*settled on* — `webgl`, or `dom` with `requested` / `unavailable` /
`context-lost` — and the E2E hook exposes it as `terminalRenderer`. Asking for
WebGL is not the same as getting it, and a test that cannot tell the difference
proves nothing.

### 3.6 The desktop had its own answer for which shell to run

Phase 1 gave `kanna-server` a Linux shell policy (`$SHELL` when it is an
absolute, executable bash or zsh; else `/bin/bash`; else `/bin/sh` with `-l`
rather than `--login`), and it was `pub(crate)`. The desktop is a different
process and could not call it, so it kept the old answer as a `/bin/zsh`
literal in `stores/sessions.ts` — a second source of truth for something the
server already decides, and simply wrong on a stock Linux image with no zsh.

The policy moved to `crates/runtime-defaults/src/login_shell.rs` unchanged
(macOS still resolves to `/bin/zsh` with the same argv, and its tests moved with
it). `kanna-desktop` exposes it as a `shell_launch` Tauri command, and
`sessions.ts` uses that for both shell tabs and the legacy PTY launch path.
`run_script` uses it instead of `$SHELL`-or-zsh. `ensure_term_init` now returns
`null` when the resolved shell is not zsh, because ZDOTDIR and the proxy rc
files behind it are zsh-only — on Linux that is the ordinary path, not an edge
case.

### 3.7 The VM crashed

Partway through the run the guest stopped responding to ping and SSH while
QEMU stayed up; `utmctl stop` and `utmctl stop --force` both failed, and the VM
had to be hard-stopped by killing the QEMU launcher and restarted. The cause is
not established — the plausible candidates are memory pressure (15 GiB, running
llvmpipe-backed GNOME plus a Rust build) and the graphics stack itself. It is
recorded because it is a fact about this workbench that anyone repeating this
work will meet, not because it is diagnosed.

## 4. What this changes about the plan

Nothing in M1 has yet contradicted the plan's structure, and one thing
sharpened it: §3.2 and §3.4 both trace to the same missing capability, so
**GPU access on the VM is now a prerequisite for M3's rendering matrix**, not a
nice-to-have. Until the VM user can open a DRM render node:

- every renderer measurement is llvmpipe, and a WebGL-vs-fallback throughput
  comparison measures two software paths;
- composited-window screenshots stay unavailable to the automated lane.

Both renderers being *functional* is the useful half of the answer and it is in
hand. The half that needs GPU access is *performance* — M3's fallback
throughput measurement — and it should not be attempted on this VM as it
stands, because it would compare two software rasterizers and read as a result.

The plan's L-sized 3–6 engineering-week envelope still looks like the right
range. Nothing found here contradicts the remaining five milestones; §3.3.3 and
§3.6 confirm two of them were correctly scoped.

## 5. Reproducing

On the Mac Studio, against the VM from the Phase 0 baseline:

```
# a real GNOME/Wayland display server, no sudo, no seat
ssh kanna-linux-vm
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
  gnome-shell --headless --virtual-monitor 1600x1000 --wayland-display kanna-p2 &

# GDK_BACKEND=wayland and *no* DISPLAY -- see the hang in section 3.3.
export WAYLAND_DISPLAY=kanna-p2 GDK_BACKEND=wayland XDG_SESSION_TYPE=wayland \
       XDG_CURRENT_DESKTOP=GNOME XDG_RUNTIME_DIR=/run/user/1000 \
       DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
export KANNA_WORKTREE=1 KANNA_DEV_PORT=1520 KANNA_WEBDRIVER_PORT=4545 \
       KANNA_MOBILE_SERVER_PORT=48220 KANNA_TRANSFER_PORT=4555

cd ~/kanna-p2 && ./kd setup --check && ./kd dev up
curl -s http://127.0.0.1:4545/status
curl -s -XPOST http://127.0.0.1:4545/session -H 'Content-Type: application/json' \
     -d '{"capabilities":{"alwaysMatch":{}}}'
```

To see the production terminal renderer rather than the dev default, drive the
session to `http://localhost:1520/?kannaTerminalRenderer=webgl` and read
`window.__KANNA_E2E__.terminalRenderer` back.

`kd` sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` by itself on a machine with no
openable render node; on one with GPU access it sets nothing.

## 6. E2E coverage

M1 is a spike, and its code changes carry unit coverage only. The lane that
would cover them end to end is M6's, and it needs the harness work M6 names;
this document is the dated note that gap requires. Present coverage:
`tools/kd/tests/tmux.test.ts` (both platforms' forwarding lists),
`tools/kd/tests/dev-plan.test.ts` (the render-node probe and its explicit
override), and
`apps/desktop/src/composables/terminalRenderer.test.ts` (request vs outcome,
including the URL request that survives a reload).

The shell policy's own tests moved with it into `kanna-runtime-defaults` and
still pass on both platforms; `sessions.ts`'s callers are covered by the
existing desktop store tests, which now stub `shell_launch`.

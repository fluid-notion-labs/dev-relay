# Dev relay — build distribution + remote log streaming

Goal: while iterating on feel (sticky aim modes, cue camera, strike detection), Nick tests on a
Windows laptop over LAN without pulling/pushing code. The dev server on the Linux build box hosts
`latest.exe` + a manifest of versions; the game self-updates to a git-hash-named binary, restarts,
and streams logs back so we can trace input/event ordering (especially cue-strike detection)
remotely.

**Topology: the Linux build box is the single build+publish+serve host. Clients — laptop (Windows)
and Linux alike — only run the self-updating game binary and stream logs back. No builds ever
happen on Windows.**

## Status: Linux slice implemented (serve, `build --target linux`, self-update, log streaming,
instrumentation) plus the crate side of M5/M7 (sessions, control channel, shot storage, MCP
endpoint). Remaining: `--target win` (M4) and the game-side bevy glue of §6a (input injector,
capture observers).

## 1. Crate (`dev-relay`, single published crate — will go to crates.io)

A general-purpose crate: build-publisher + LAN file server + log relay + MCP query endpoint.
Nothing in it is billiards-specific — bins, manifests, logs — reusable for any game/tool project.

```
billiards-rs/            # the game (game-side glue only, see §3/§4)
crates/dev-relay/        # published crate: the tool + a small client-side library
```

Inside the one crate:

- `dev-relay` (bin) — `serve` / `build` subcommands; tool side, runs on the Linux build box
- `dev_relay::client` — game-side library: self-update check + remote log sink
  (`std::net::TcpStream`, no async runtime)
- Bin named `dev-relay`; lib name `dev_relay` (single-crate client+tool, no fake split;
  check crates.io name availability before publishing).
- Server side: std only (hand-rolled HTTP, no TLS — LAN only). MCP side: `serde_json` for
  JSON-RPC. Client lib: std only.
- Server dir layout, default `./relay-dist/` (CLI-overridable):

```
relay-dist/
  manifest.json          # {"latest":{"":"<name>","exe":"<name>"},"files":[{"name":..,"mtime":ms,"size":n}]}
  bins/<name>[.exe]      # immutable, one per built version per platform (name = sha or sha-dN, below)
  latest, latest.exe     # convenience symlinks to the newest entry per suffix
  incoming/              # drop zone the build subcommand writes into, watcher consumes
  logs/<name>[.exe].jsonl # per-binary-version log lines (JSONL, append)
  inputs/<name>.jsonl    # (M5) input commands the agent enqueued, JSONL, append
  shots/<name>/<session>/ # (M5) captures bucketed per session: <seq>.png / <seq>.depthbin
```

`manifest.json` `latest` is keyed by suffix (`""` = Linux, `"exe"` = Windows) because one
manifest serves both platforms and a single `latest` field could not. Clients compare only
against their own suffix's entry; `files` is the full scan of `bins/` sorted by name.

Bin-name grammar: `^[a-f0-9]{4,40}(-d[0-9]{1,4})?(\.exe)?$` — enforced everywhere a client
supplied name touches the filesystem (log query param, `/bins/`, `/logs/`, `/control`, `/shot`).
Session-id grammar: `^[a-z0-9]{6,16}$` (client-minted, see §4) — enforced wherever a session id
touches the filesystem (`shots/` paths, `/control`, `/shot`); inside log lines it stays opaque.

## 2. Endpoints & publishing

### `dev-relay serve` (HTTP server on LAN)

Plain std server on `0.0.0.0:<port>` (default `8642`), thread-per-connection, `Connection: close`:

- `GET /manifest.json` — serve the file the watcher keeps fresh
- `GET /bins/<name>` — serve a published binary
- `GET /latest`, `/latest.exe` — convenience pointer bytes
- `POST /log?bin=<sha-or-name>` — append JSONL body to `logs/<bin>.jsonl` (bin validated
  against the bin-name grammar, body capped at 4 MiB, `logs/` mkdir on demand, ignore
  write errors)
- `GET /logs/<name>` — serve the JSONL back (browser/ssh tail; name validated same as above)
- `GET /control?bin=<name>&session=<sid>` — long-poll the agent's command queue for one
  session (§6a)
- `POST /shot?bin=<name>&session=<sid>&kind=color|depth&seq=<n>` — capture drop, written to
  `shots/<bin>/<session>/`; `GET /shots/<bin>/<session>/<seq>.(png|depthbin|depth.json)` serves
  them back (§6a)
- `POST /mcp` — see §6

### Publishing flow (server publishes, build only drops)

- `dev-relay build [--target linux|win] [--bin NAME] [--project DIR] [--dist DIR]` (target
  default `linux`; `--bin` defaults to the package name read from the project's
  `cargo metadata`; `--project` defaults to cwd) — runs cargo, then drops the binary:
  - `win`: `cargo build --release --target x86_64-pc-windows-gnu` (deferred, M4)
  - `linux`: `cargo build --release`
  - verify target binary exists (`target/release/<bin>[.exe]`) with a plain error otherwise
  - version name from git:
    - clean tree → `<short-sha>[.exe]` via `git rev-parse --short HEAD`
    - dirty tree → `<short-sha>-d<N>[.exe]` where `N` = 1 + count of existing
      `bins/<sha>-d*` entries with the same suffix — every dirty rebuild is a *distinct*
      version so the client always sees progress without a commit; clean rebuilds of the
      same sha reuse the name and the watcher's rename-over replaces the file
    - platform is distinguished by `.exe` suffix only
  - write `incoming/.tmp-<name>` then rename within `incoming/` (atomic same-directory;
    watcher only ever sees complete files)
- Watcher (inside the running `serve`, thread host): poll `incoming/` every ~1 s (mtime+size, no
  inotify dep) → move to `bins/` (rename-over if the name exists — same-name rebuilds only
  happen on a clean tree), regenerate `manifest.json` from a `bins/` scan (write temp +
  atomically rename), refresh `latest` / `latest.exe` symlinks (relative, `bins/<name>`) to
  the newest entry per suffix. An initial pass runs at `serve` startup.
- `dev-relay serve` prints the LAN `http://<ip>:<port>` line on start (LAN IP via a no-send
  UDP connect; `0.0.0.0` fallback).
- Windows participates only as a *consumer*: it never compiles, never hosts — it just runs a
  downloaded exe with `--update-url` and streams logs.

### Cross-compile prerequisites (Linux → Windows, one-time) — DEFERRED, M4

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install mingw-w64          # provides x86_64-w64-mingw32-gcc on PATH
```

- `dev-relay build --target win` verifies both are present (rustup target list + mingw gcc on
  PATH) and fails with a plain-language error otherwise.
- First target build is a full dependency recompile (slow); later ones incremental.
- Fallbacks if apt unavailable: `--builder cross|mingw|xwin` flag, default `mingw`.

## 3. Self-update in the game (via `dev_relay::client`)

Game-side activation: only when `--update-url <url>` (or env `BILLIARDS_UPDATE_URL`) is passed;
completely inert otherwise so `cargo run` development is untouched.

- Game build.rs stamps `DEV_RELAY_GIT_SHA` (`git rev-parse --short HEAD`,
  `cargo:rerun-if-changed=.git/HEAD`, `"unknown"` outside a repo); `git_sha!()` reads it.
- On startup (before heavy init): `GET {url}/manifest.json`.
  - Compare the `latest` entry for the *own suffix* with (a) `git_sha!()` baked at compile
    time and (b) the running exe's filename stem if it matches the bin-name grammar
    (filename wins if both available, so the same source tree can have older copies on disk).
  - If up to date → continue, start the log streamer.
  - If newer → download `bins/<latest>` to `<binary_dir>/<latest>` next to the current
    exe (never in-place; suffix matches the current exe's), chmod +x on Linux, spawn the new exe
    with the same args (`--updated-from <old>` added, previous `--updated-from` stripped),
    sleep ~0.3 s, exit the parent.
- Loop guard (cross-process state in `<binary_dir>`): before spawning, the parent writes
  `.relay-pending` containing the target name; the new exe runs a watchdog that deletes it
  after 5 s uptime and resets `.relay-fails` to 0. On startup, a *stale* `.relay-pending`
  (name ≠ own) means the previous update died early → increment `.relay-fails`; at ≥ 2
  refuse further self-updates and surface an error (log + stderr) instead of updating.
- Player loop: run `dev-relay build`, then just relaunch the game — it hot-swaps itself.

## 4. Log streaming (game → LAN → build box)

- The game already logs via `info!`/`debug!` (bevy_log/tracing). Game side `src/relaylog.rs`
  defines a small `tracing_subscriber::Layer` impl that converts events into
  `dev_relay::client` log lines (the tracing-subscriber dep lives in the *game*, keeping the
  published crate std-only) and pushes into the sink channel; the crate-side sink thread
  assigns `seq` (AtomicU64, in-process monotonic) and batches JSON lines
  (`{ts, level, target, msg, seq, fields, session}`), POSTing to `/log?bin=<name>` every
  ~0.5 s or 32 records, whichever first. Fire-and-forget; never blocks gameplay; silent
  unless the app knows the update URL. Best-effort flush on exit.
- **Sessions — the correlation bucket (implemented)**: the client mints one session id per
  process on first relay activity (time+pid hash, grammar in §1, no RNG dep), stamps it on
  every log line and the `run/launch` marker, and carries it across self-update spawns via
  `--relay-session <id>` (minted if absent, strip-and-readd alongside `--updated-from`) — so
  one playtest = one bucket even across hot-swaps. Injected inputs, screenshots, and depth
  captures all key off the same id (§6a). The field is additive: the server appends log
  bodies opaquely, so lines/clients without it keep working.
- Registered through `LogPlugin { filter, custom_layer }` (bevy 0.19 exposes
  `custom_layer: fn(&mut App) -> Option<BoxedLayer>`); the layer only exists when the sink
  was started, and events reach it only after the `EnvFilter` — so `--verbose-dev` gating is
  just the filter string.
- `LogPlugin` config: default filters as-is; when `--verbose-dev` is passed, set
  `filter = "debug,wgpu_core=warn,wgpu_hal=warn,{bevy DEFAULT_FILTER}"` (game code at
  `debug!`, graphics stacks silenced) and pass `--verbose-dev` through to the relayed exe so
  it preserves the setting.
- Synthetic markers on startup (`run/launch`) and on any `PhysicsBackend` switch — so we always
  know which engine was live when a trace appeared.

## 5. Instrumentation map (what gets logged, where)

- `src/aim.rs::aim_input`
  - `debug!` on every just-pressed mode change (W/E/T/Q/X) → resulting mode
  - pull mode per frame: `debug!(dy, pull, drive_vel)` — throttled (emit only when pull changed
    > 0.0005 or drive_vel crossed sign) to keep log volume sane
  - pull-floor clamp crossing in `aim_input` (`next < CUE_MIN_PULL`): `debug!` with raw dy,
    drive_vel, pull (the actual fire gate lives in `pull_strike`: reject when
    `pull > CUE_MIN_PULL || drive_vel <= 0`)
- `src/aim.rs::pull_strike`
  - rejection *near*-fire path: `debug!` when `pull > CUE_MIN_PULL` but `drive_vel` decays or
    `pull` resets — pin down the hit-and-miss source
  - actual fire: `info!(dir, strength, contact)` (drives through to both backends)
- `src/physics/custom.rs::apply_strike` and tailuge equivalent
  - `debug!` enter/exit with dir + strength
- `src/aim.rs` cursor visibility transitions → `debug!`
- `src/camera.rs` mode changes (Cue/Free/stand) → `debug!`
- `src/dev_ui.rs` inspector open/close, screenshot trigger, backend switch → `info!`

All cost nothing when the sink isn't active (game-side shim only pushes if update-url present;
macros compile to no-ops under `log` if levels are filtered).

## 6. MCP endpoint (fast debug loop from opencode) — implemented (crate side)

`POST /mcp` implements MCP streamable-HTTP JSON-RPC (POST in → JSON out; no SSE):
minimal tool surface only, id-based request/response, `initialize`/`initialized` handshake with a
generated `Mcp-Session-Id` response header, `notifications` answered 202-empty.

All results are structured, never prose: typed JSON objects, snake_case keys mirroring the JSONL
line fields, lists under a named key, numbers as numbers — so agents chain tools programmatically
without text-parsing. Every tool takes optional `bin` + `session` (defaults: latest manifest bin;
newest session), and live-target tools (`send_input` & co, `screenshot`, `depth`) default to the
newest *active* session — one with a live control poll. Tools:

- `list_bins {}` → `{ latest: {"": <name>, "exe": <name>}, files: [{name, mtime, size}] }`
- `sessions { bin? }` → `{ sessions: [{bin, session, first_ts, last_ts, log_lines, inputs,
  shots}] }` — index derived at query time by scanning `logs/`, `inputs/`, `shots/`; files are
  the truth, no index to keep fresh, scans are cheap at LAN scale
- `tail_logs { bin?, session?, max_lines=200, level_min?, contains? }` → `{ lines: [...] }` —
  newest N matching lines of `logs/<bin>.jsonl` (default bin = latest manifest entry; omit
  `session` to span all runs, pass it to bucket), lines returned verbatim
- `session_events { bin?, session?, since_ts? }` → `{ events: [...] }` — the bucket view:
  log lines, enqueued input commands, and shot/depth captures of one session merged into a
  single ts-sorted stream, each `{kind: "log"|"input"|"shot", ...}` (shots carry the client
  `seq`, inputs carry cmd ids echoed by the in-game `input/apply` marker, so ordering
  survives client/server clock skew)

This lets opencode (or any MCP client) query logs directly instead of grepping JSONL by hand.

## 6a. Agent-in-the-loop: input driving + screenshot/depth capture — relay+client side done, game-side bevy glue pending (billiards-rs pass)

Goal: opencode (or any MCP client) can *play* the running game — send keystrokes and mouse,
see the result as a screenshot or depth buffer — closing the loop that
`docs/interaction.md` defines. The app under test stays a plain game binary on the laptop;
all remote control flows through the dev-relay server, mirroring the log-streaming topology
(reversed direction).

Implemented in the crate: `dev_relay::client::ControlChannel` (long-poll loop + typed
`ControlCmd`), `dev_relay::client::post_shot`, the server `/control` queue with per-command
records in `inputs/`, `POST /shot` + `GET /shots` storage, and the MCP tools of §6
(`send_input`, `send_input_mouse`, `click`, `screenshot`, `depth`, `recent_shots`) — all
unit-tested and live-checked with a curl "game". Remaining (game side): the `input_inject.rs`
system, the `ScreenshotCaptured` observer that POSTs instead of `save_to_disk`, and the depth
prepass capture camera.

```
opencode --MCP--> dev-relay server ----------> relays commands to the app over its control channel
   ^          (relay-dist host :8642)             (same TCP patterns as the log sink, reversed)
   └--- screenshots / depth files stored in relay-dist, served over HTTP --- from the app: POST /shot
```

### Prior art (checked against the vendored bevy 0.19.1 tree)

- **BRP — `bevy_remote` (official)**: JSON-RPC over HTTP (default `127.0.0.1:15702` + render-subapp
  port), built-in methods for ECS/resource access (`world.get_components`, `world.query`,
  `world.spawn_entity`, `world.write_message`, observe/watch variants, `rpc.discover`,
  `registry.schema`), extensible via `RemotePlugin::with_method`. `vendor/bevy/examples/remote/integration_test.rs`
  is the closest prior art to this entire feature: it connects to a running app over BRP,
  **spawns `Screenshot::primary_window`, observes `ScreenshotCaptured` to fetch the PNG**
  (`bevy_render/src/view/window/screenshot.rs` — bevy core screenshot API since 0.15), and
  **drives input by writing `WindowEvent` messages** (`world.write_message` with
  `CursorMoved`, then `MouseButtonInput` pressed + released). Windows must not be fully
  occluded or the GPU renders nothing (screenshot is black).
- **Capture**: our `src/dev_ui.rs` already uses the same core API (screenshot button →
  `Screenshot::primary_window()` + `save_to_disk`). For depth: bevy core pipeline
  `DepthPrepass` (and `DepthPrepassDoubleBuffer` for querying the previous frame) writes the
  depth to a texture readable via `bevy_render/src/gpu_readback.rs` (`Readback::texture`
  → `ReadbackComplete::to_shader_type`); the prepass depth readback exists in
  `bevy_core_pipeline/src/prepass`. Note `core_3d` docs: `copy_texture_to_texture` for depth
  is unsupported — depth export must go through the prepass texture, not a copy.
- **Not adopted**: BRP itself — `127.0.0.1:15702` opens up the whole ECS; we want a narrow
  dev-relay control surface instead, and no extra dep stack in the game (the game-side
  control link is plain `TcpStream`, std only; BRP drags in hyper/smol). BRP remains the
  proven spec to borrow message shapes from.

### Design (how it plugs into what exists)

- **Control channel (relay → app)**: new `dev_relay::client::ControlChannel` — a background
  thread that long-polls `GET /control?bin=<name>&session=<sid>` on the dev-relay server (or a
  persistent TCP read loop; long-poll fits the existing hand-rolled server best: server executes
  the queued commands, streaming them as one JSON array and hanging until commands drain or a
  ~5 s timeout). Server endpoint acts as the agent's queue, keyed per (bin, session); every
  enqueued command is also appended to `inputs/<bin>.jsonl` (`{ts, session, id, cmd}`) — the
  durable record that ties inputs into the session bucket. Inert unless `--update-url`.
- **Input injection seam (bevy 0.19)**: a small `input_inject.rs` system runs early in
  `PreUpdate`, draining a channel of `InjectedInput { key down/up, mouse_button, mouse
  delta, wheel }` commands decoded from the control channel, and writes
  `KeyboardInput`, `MouseButtonInput`, `MouseMotion` messages (same shape bevy_winit
  produces) — `bevy_input`'s aggregation systems then feed `ButtonInput` /
  `AccumulatedMouseMotion` exactly as if a real mouse/key did it, so game code needs zero
  changes (BRP's integration test does the same via `world.write_message` of `WindowEvent`).
  Held keys need edge tracking in the injector (release when the agent sends `up` or the
  channel drops, so no stuck keys). egui panels receive events like real input does. The
  injector logs each applied batch as a `debug!` `input/apply` marker (command ids), so
  inputs appear in the session's own log stream in client time; the server-side `inputs/`
  record (server ts) is the fallback join.
- **Screenshots**: same `Screenshot::primary_window()` API, but instead of `save_to_disk`
  the observer drains `ScreenshotCaptured` and POSTs the PNG body to
  `POST /shot?bin=<name>&session=<sid>&kind=color&seq=<n>` — written to
  `relay-dist/shots/<bin>/<session>/<seq>.png`. The capture reuses the log `seq` counter
  (the same AtomicU64), so shots and log lines of one session interleave in a single
  client-side order — no separate shot numbering, no clock-skew guessing.
  Resolution note: capture at the primary window's resolution; agent can request a
  smaller window first via agent-set `--size` at launch if needed.
- **Depth buffer**: enable `DepthPrepass` per-capture, not persistently (adding/removing it
  live modifies the render graph — do it by spawning a second hidden capture camera with
  the prepass component only when a depth request is queued). Read back the prepass depth
  (`gpu_readback`), ship raw f32 depth plus a JSON header (width, height, near, far,
  projection) instead of a fancy image, so the consuming agent just does `np.fromfile` +
  reshape. The MCP tool returns both the header and a
  `GET /shots/<bin>/<session>/<seq>.depthbin` URL. Color PNG handles the eyeball case;
  raw depth is what AI consumers want.

### New MCP tools (extends §6)

- `send_input { key: "w", action: "down"|"up", bin?, session? }`,
  `send_input_mouse { dx, dy, wheel?, bin?, session? }`,
  `click { button, action, bin?, session? }` → `{ session, queued }` — enqueued on the
  control channel for the target session (default: newest active session; explicit
  `bin`+`session` when several clients run concurrently)
- `screenshot { bin?, session? } → { session, seq, url, width, height, base64? }` — URLs
  point at the relay server, so opencode binary image tools can fetch them like any file;
  tool also returns base64 inline when the image is < ~1.5 MB for clients that can't fetch
  URLs
- `depth { bin?, session? } → { session, seq, url, width, height, near, far, projection }`
- `recent_shots { bin?, session?, max=10 } → { shots: [{session, seq, kind, size, ts}] }`

### Scope guards for this feature

- No new deps in the game for this (TCPStream + already-present render features); the MCP
  side keeps using `serde_json` only.
- Input is injected at the same points a real OS would produce events, so physics-gameplay
  timing (60 Hz FixedUpdate) is unaffected; commands are consumed in server-received order,
  never re-timed into "frame perfect" inputs (real enough for feel iteration).
- Screenshots never block the frame: POST from the observer thread via the existing sink
  machinery pattern (non-blocking channel) — if the POST fails the shot is dropped.
- Session id is the only correlation state: no in-memory registry — everything a session
  touches (log lines, input records, shot files) is keyed by (bin, session) on disk, so
  buckets survive server restarts and are inspectable by hand.

## 7. Staged verification

1. First verify the whole loop on the Linux box alone: `dev-relay serve` + `dev-relay build
   --target linux` (two terminals or one `bash -c 'serve & sleep 1; build'`); run the game with
   `--update-url http://127.0.0.1:8642` — it self-updates to `bins/<gitsha>` and streams logs.
   Nothing Windows-specific has been touched yet.
2. Then `dev-relay build --target win` from the same box, and the laptop self-updates + streams
   logs. Windows client needs only the downloaded exe + `--update-url`.
3. opencode drives `tail_logs` during this to confirm MCP gives a usable debug loop.

Per-OS differences in the game itself are tiny:
- spawn semantics on Windows (simple `Command::spawn`); the parent must exit *after* the child has
  started (small `std::thread::sleep` then `std::process::exit`)
- path/exe-name suffix detection (`<sha>.exe` vs `<sha>`)
- log sink uses `std::net::TcpStream` (no async runtime), identical on both OSes

The client never builds anything: on Windows the user only ever runs the downloaded exe; old
`<sha>` bins stay on disk so an old client picking up the new manifest needs only the new sha,
not a rebuild.

## 8. Scope guards

- No change to `FixedUpdate` schedule or the physics contract — update + logging are orthogonal.
- `--update-url` is a runtime arg, never baked-in — normal `cargo run` development untouched.
- Selfupdate code always compiled but fully inert without the flag (~100 lines, no feature flag).
- No comments convention (AGENTS.md) — keep code terse, explanations live here.
- This file is the source of truth for what to build; implementation follows it.

## Milestones

1. DONE — `dev-relay serve` + `POST /log` + `manifest.json` (unit-tested + live-checked on 127.0.0.1)
2. DONE — `dev-relay build --target linux` self-update round-trip, verified on the build box:
   build → watcher publish → manifest → download → relaunch → loop guard (refusal after 2
   early-exit crashes) → `run/launch` marker streamed. The child only died on headless
   display init (environmental); gameplay-level confirmation is the user checkpoint on
   real hardware.
3. DONE — fine-grained logging landed (cue strike etc.); volume/verbosity confirmed by the
   user in a real session
4. DEFERRED — `dev-relay build --target win` (mingw toolchain) + laptop self-update verified
   end-to-end
5. DONE (crate side) — MCP endpoint + opencode-driven debug loop: structured results,
   session bucketing, `/control` + `/shot` + `/shots` endpoints, all ten tools unit-tested
   and live-checked on 127.0.0.1 (initialize handshake, send_input → long-poll delivery,
   screenshot/depth round-trip, merged session_events)
6. DEFERRED (optional) — web tail page instead of ssh tail
7. PARTIAL — agent-in-the-loop (§6a): relay + client-library side done (ControlChannel,
   post_shot, queue + inputs records, shot storage); remaining is the game-side bevy glue
   (input injector, capture observers) — logs, inputs, and shots share one session id (§4)

Implementation notes (verified live): startup markers (`run/launch`, self-update failure) are
POSTed synchronously via `dev_relay::client::post_line` *before* engine init so they survive a
crash-on-boot — the async sink is fire-and-forget and its in-flight batch is lost on panic.
Session/mcp notes: fs mtime uses the coarse kernel clock and can lag `SystemTime::now` by a
tick, so ts-ordering across sources in `session_events` is best-effort — the client `seq`
(logs vs captures) and command ids (inputs, echoed by the in-game `input/apply` marker) are the
reliable joins. The depth sidecar (`<seq>.depth.json`) is written before its `.depthbin` so a
waiting `depth` tool call never observes a capture without its header.

# Requirement: client launcher shim

Status: **implemented** (dev-relay `launcher` module + `relay-launcher` bin;
game-side `billiards-rs/src/relay.rs`). Deviations from the original plan,
agreed during implementation:

- loop mode was built now (not deferred): `relay-launcher` polls the manifest
  (default every 15 s) and stops/respawns the game when a newer build appears
- the launcher logs under its **own bin id** `relay-launcher` (own session),
  visible in MCP tools like any game session — the server's bin-name grammar
  was generalized to allow non-sha ids (`^[a-z0-9][a-z0-9-]{2,38}[a-z0-9]$`)
- the game went **fully assetless** (procedural red/yellow balls, no GLTF/HDR/
  textures), so the assets-distribution question is moot; the launcher sets
  cwd to its own dir, nothing else ships alongside the binary
- game flags renamed: `--relay-url`/`BILLIARDS_RELAY_URL` (+ legacy
  `--update-url`/`BILLIARDS_UPDATE_URL` still accepted); the launcher passes
  `--relay-bin` and `--relay-session` so the game logs under the exact
  artifact the server published
- crash guard: child dies within 10 s twice → launcher posts ERROR under
  `relay-launcher` and holds (keeps polling) until a newer build is published

## Problem

Today the game binary itself self-updates (`billiards-rs/src/selfupdate.rs` +
`dev-relay::client::check_and_update`). In practice this has proven fragile:

- the running exe rewrites itself in its own directory, with a crash-guard
  (`.relay-pending` / `.relay-fails`) that can wedge into "self-update refused"
- versioning is tied to the exe's file name (sha + dirty suffix), so a
  renamed/copied exe loses its identity (observed: the client stayed on
  `bfc3f9c-d1` forever and never picked up `d2`/`d3`)
- the watch-exec chain (binary spawns a newer copy of itself, which spawns a
  watchdog, converts its own session, ...) is hard to reason about
- observed symptom on the laptop client: surprisingly long startup, and
  updates silently never apply

## Desired design

A small standalone **shim / launcher binary** ships to the client *once* (hand
copied, not self-updated). From then on it owns updating; the game binary
never touches its own file system identity.

### Responsibilities split

- **shim** (`relay-launcher`, lives in dev-relay as a second bin target)
  - fetches `/manifest.json`, compares against the currently running game
    build, downloads `/bins/<name>` to its own managed directory, installs it
    (atomic rename + exec bit), spawns the game
  - owns the "latest" decision and retries/backoff on download failure
  - keeps passing through user-provided game args, and adds
    `--update-url <relay>` (or equivalent env) so the game knows where to
    log/attach
  - optional, later: long-running loop mode — poll the manifest periodically
    and restart a running game when a new build appears
- **game binary**
  - no self-update logic at all; it only consumes a relay URL for
    **logging + control input** (`LogSink`, `ControlChannel` stay in-game)
  - identifies itself for logging purposes by a stable build id baked in at
    build time (`DEV_RELAY_GIT_SHA` artifact suffix), not by exe file name

### Naming / identity

- game exe names may stay sha-suffixed, but the shim owns name selection and
  the directory layout (e.g. `shim-dir/bins/<name>`), so a stale copy lying
  around can never be confused with the update decision
- the shim itself has no version-sensitive state; its only config is the
  relay URL (flag or env, e.g. `BILLIARDS_RELAY_URL`)

### Non-goals

- self-updating the shim
- Windows-specific install tricks beyond the existing exec-bit handling
- replacing the relay server; `/manifest.json`, `/bins/<name>`, `/log`,
  `/control` endpoints stay as-is

## Acceptance

1. fresh client: shim + relay URL → shim downloads the current latest and
   starts the game; logs appear under the new bin id on `/log`
2. publish a newer build → next shim (re)launch runs the new build without
   any manual steps on the client
3. removing the relay URL env/flag → game still runs and starts (relay
   features degrade silently), shim does not crash-loop
4. crash-guard behavior: a build that fails to boot twice is reported via
   logs and not retried in perpetuity
5. the game binary alone, launched directly, still works (no update logic
   needed, just slower/no auto-update)

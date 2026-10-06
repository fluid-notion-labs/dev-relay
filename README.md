# dev-relay

LAN build distribution + remote log streaming, plus a tiny client library for
self-updating apps. Nothing in the crate is game-specific — bins, manifests,
logs — reusable for any tool or game project. Design docs live in the repo at
`docs/dev-relay.md`.

Crate layout: the `[lib]` is `dev_relay`, the tool binary is `dev-relay`
(`src/bin/dev-relay.rs`).

- Tool side: std only, hand-rolled HTTP, LAN only (no TLS, no async runtime)
- Client side (`dev_relay::client`): std only, `TcpStream`, no async runtime
- MCP side: `serde_json`

## Tool binary (`dev-relay`)

```
cargo run -p dev-relay -- serve
cargo run -p dev-relay -- build
```

### `dev-relay serve [--dist DIR] [--port N]`

HTTP server on LAN (default port `8642`, dist default `./relay-dist/`):

| Endpoint | Behaviour |
|---|---|
| `GET /manifest.json` | watcher-refreshed manifest of published bins |
| `GET /bins/<name>` | published binary |
| `GET /latest`, `/latest.exe` | convenience pointer bytes (newest per suffix) |
| `POST /log?bin=<name>` | append JSONL body to `logs/<bin>.jsonl` (4 MiB cap) |
| `GET /logs/<name>` | serve the JSONL back |
| `POST /mcp` | deferred (future milestone) |

`serve` spawns a watcher thread that polls `incoming/` (~1 s), promotes new
files into `bins/`, regenerates `manifest.json`, refreshes the `latest`
symlinks, and prints the LAN `http://<ip>:<port>` line on start.

### `dev-relay build [--target linux|win] [--bin NAME] [--project DIR] [--dist DIR]`

Runs a release build of the project (`--project`, default cwd; ` win` target
deferred) and drops the binary into `<dist>/incoming/`. The version name comes
from git: clean tree → `<short-sha>`, dirty tree → `<short-sha>-d<N>` where N
counts existing dirty entries (distinct names per dirty rebuild so clients
always see progress). The running `serve` publishes it.

Server directory layout (default `./relay-dist/`):

```
manifest.json            {"latest":{"":"<name>","exe":"<name>.exe"},"files":[...]}
bins/<name>[.exe]        immutable, one per built version per platform
latest, latest.exe       symlinks to the newest entry per suffix
incoming/                drop zone, watcher consumes
logs/<name>.jsonl        per-binary log lines, appended by clients
```

## Client library (`dev_relay::client`)

For apps that self-update and stream logs back — in this repo the game wires
it via `src/selfupdate.rs` / `src/relaylog.rs`.

- Update check: `check_and_update(base_url, ...) → UpdateContext` — compares
  `latest` for the running exe's suffix (`.exe` → Windows) against the
  `DEV_RELAY_GIT_SHA` stamped by build.rs (`git_sha!` macro) and the running
  exe's own filename; downloads a newer `bins/<name>` next to the current exe,
  spawns it with the same args (`--updated-from <old>`), and the caller exits.
  Cross-process crash guard: `.relay-pending` / `.relay-fails` in `<binary_dir>`
  — ≥ 2 early-exit crashes refuse further self-updates.
- Log sink: `LogSink::start(base_url, bin)` spawns a background thread that
  batches `{ts, level, target, msg, seq, fields}` JSON lines and POSTs them to
  `/log?bin=` every ~0.5 s or 32 records (fire-and-forget, never blocks the
  app; `flush()` for a synchronous best-effort drain on exit).
- Startup markers that must survive a crash-on-boot: `post_line` posts one
  record synchronously, before engine init.

Fully inert unless the app passes an `--update-url` / `BILLIARDS_UPDATE_URL`.

## Source map

| Module | Role |
|---|---|
| `server.rs` | HTTP endpoints, watcher thread, dist layout |
| `build.rs` | `build` subcommand: cargo invocation, drop into `incoming/` |
| `manifest.rs` | manifest read/write, `latest`-suffix comparison |
| `naming.rs` | bin-name grammar `^[a-f0-9]{4,40}(-d[0-9]{1,4})?(\.exe)?$`, enforced on every client-supplied path |
| `httpc.rs` | minimal HTTP client (`Base` parse + request) |
| `client.rs` | `check_and_update`, LogSink, flush/post_line |

# Command-pattern input injection (dev-relay, agent-in-the-loop)

Companion to `docs/dev-relay.md` §6a. This file is the source of truth for the
command-injection mode of the agent-in-the-loop: instead of forging raw key/mouse events
over the network, the MCP client emits **game commands** — bevy Message enums with data
attached — that the app's behaviour systems consume directly.

## The idea

The app keeps a normal interaction model (`docs/interaction.md` in the project): input
handling stays *chatty* and dumb — keys + mouse deltas are mapped to Commands, and the
actual code that does the actions just follows Commands.

- Commands keep intention and behaviour clear: `RotateAroundBall(Vec2)` says what was
  intended; nothing else needs to re-derive it from raw deltas.
- Their producer is swappable: keyboard/mouse today, network commands, scripted replays,
  or an AI agent — all feed the same Command channels (`write` from anywhere).
- The dev-relay MCP can then export the app's supported Commands and generate them,
  instead of having the agent know key bindings. Raw-input forging stays supported
  (needed for anything that bypasses the mapping), but Commands are the preferred mode.

Note on vocabulary: this is NOT bevy's `Commands` (deferred structural ECS ops — spawn,
insert, despawn). This is an intent-message pattern at the game level.

## Prior art (searched in the vendored trees)

### `smooth-bevy-cameras` (~oct 2026: `vendor/smooth-bevy-cameras`)

- `controllers/orbit.rs:92` / `controllers/unreal.rs:108` — generic
  `ControlMessage` enum, depends only on bevy_{math,ecs}: `Rotate(Vec2)`, `Pan(Vec2)`,
  `TranslateEye(Vec2)`, `Locomotion(Vec3)`.
- `unreal.rs:117-229`: the *input* system is a dumb translator — reads
  `MessageReader<MouseMotion>` / `MouseWheel` / keyboard and writes
  `ControlMessage::Rotate(...)`, `::TranslateEye(...)`, `::Locomotion(...)`; it owns no
  camera state and does no behaviour at all.
- `unreal.rs:234-263` — the *mechanics* system reads only `MessageReader<ControlMessage>`
  and applies them to the `LookTransform`; smoothing is orthogonal (`Smoother`).
- This is a working, published example of the whole loop: chatty input stays primitive,
  the decision-relevant behaviour is a small enum-matching actuator.

### `bevy_remote` BRP (official, `vendor/bevy/crates/bevy_remote`)

- `world.write_message` (`builtin_methods.rs:96`) sends arbitrary reflectable messages
  from a remote client — the mechanism Commands would ride on if we did not use our own
  control channel; used exactly this way by `examples/remote/integration_test.rs`
  (which messages `WindowEvent`s to fake a click).
- `rpc.discover` + `registry.schema` export a machine-readable surface of method params
  and registered types — the model for a `commands` catalogue export.

### `leafwing-input-manager` (community convention, not vendored)

- De-facto "bevy way": a game-defined `Action` enum + `InputMap` translating inputs,
  `ActionState` querying behaviour; test support centrifuges at injecting the *virtual*
  state/press of actions (`ActionState::press(action)`) rather than forging raw events.
  Shows the ecosystem idiom: actions are the test seam, inputs are replaceable.

### Raw-event forging (kept as fallback)

bevy input aggregation (`bevy_input`) consumes `KeyboardInput` / `MouseButtonInput` /
`MouseMotion` / `MouseWheel` messages (the same shapes `bevy_winit` produces) →
`ButtonInput` / `AccumulatedMouseMotion`. Writing those messages directly (or BRP
`world.write_message` of `WindowEvent`) works, but reaches only code that reads
raw-ish events/view of them and duplicates the app's key mappings in the agent.

## Design

### Game side: a command registry

- The game defines its interaction-model Commands as
  `pub enum GameCommand` variants; a tiny registry maps
  name → (param JSON schema, channel/tag, doc string).
- Every remote-compatible command has: documented semantics, sourced from
  `docs/interaction.md`, plus range/impact notes
  (e.g. "same behaviour as holding Space — charges per tick").
- On startup (when `--update-url` is present) the app POSTs its
  `commands.json` catalogue (name, schema, doc); the dev-relay caches it
  so MCP clients can discover the command surface at any time.

### Control channel transports Commands

The log channel (`POST /log?bin=`) is app → server only; the *control* channel (server →
app long-poll, §6a) transports commands both ways: raw `InjectedInput` rows AND
`GameCommand` rows (JSON `{ name, params, seq }`). The app injects commands via a
`MessageWriter<GameCommand>` in the same `PreUpdate` drain as the raw
injection path, and the behaviour systems treat network Commands like local input
without distinguishing the source.

### dev-relay / MCP surface

- `commands.json` is fetched from the dist as the game's schema catalogue,
  exposed over MCP and used for codegen/autodoc.
- `list_commands { bin? }` → catalogue (name, params schema, doc, example).
- `send_command { bin?, name, params }` → server enqueues on the control channel
  for the app; app writes it as `GameCommand` next frame.
- Validation: unknown names or schema-mismatched params rejected server-side
  before they ever reach the game's command channels.
- `send_input`/`click` (forging; §6a) stay, for anything outside the mapping
  (dead-zone bugs, hidden key chords, replay-what-the-human-did).

### Why prefer Commands over forging

- Fidelity: the agent plays in the app's own language (same vocabulary as
  `docs/interaction.md`), not a re-encoding of key bindings.
- Reliability: no timing sensitivity to bevy PhysInput aggregation or to another
  panel's input focus; messages are explicit, not reproduced pointer/threading ambience.
- Observability: the app logs `GameCommand` writes (`debug!`, name + params, via the
  §4 sink), so every agent action appears interleaved with gameplay traces.

### Scope guards

- Raw injection (§6a) remains supported; Commands route through a *separate*
  enum-plus-registry layer — no overstitching of the input path.
- The catalogue compiler/codegen never needs the game's source tree — schema
  travels via `commands.json` only.
- GameCommands are inert unless the update URL is present (same gate as §3).

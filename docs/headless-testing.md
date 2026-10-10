# Feature request: headless mode with screenshot-to-texture

**Status:** proposed
**Scope:** `billiards-rs` (client) + `dev-relay` (launcher/serve)

## Problem

Automated testing through the live game client has two pain points:

1. **No headless run mode.** Verifying a physics/camera fix end-to-end today
   requires the full windowed client running on a machine with a display
   (currently the laptop). A CI box or a SSH session can't run the game, so
   regression checks stay manual.
2. **Focus stealing.** The windowed client takes keyboard/mouse focus, and the
   developer sharing that machine gets interrupted ("sometimes I accidentally
   focus on the game" — clicking into it while working). A headless client
   would run with no window at all, so this can't happen.

## Proposed design

### Client (`billiards-rs`)

- New CLI flag / env var, e.g. `BILLIARDS_HEADLESS=1` (picked up by
  `relay_control` / `main.rs`), that:
  - disables window creation (WGPU headless surface, or an offscreen render
    target), keeping the ECS schedules, physics, and camera systems running
    unchanged;
  - replaces `screenshot` capture with **render-to-texture**: the `screenshot`
    control command renders the current frame into an offscreen texture
    (same camera, same resolution), reads it back, and posts it through the
    existing `shot/upload` path — so `dev-relay screenshot` works unchanged;
  - skips systems that need a real window (`bevy_winit`, cursor grabs, the
    fullscreen toggle, `S`/`F11` window handling); the dev panel / egui can be
    disabled or kept if it renders to texture fine;
  - keeps the control channel (`/control` long-poll), log sink, and data
    buckets identical, so all existing MCP tooling works as-is.
- Launcher interplay: `relay-launcher` spawns the client with
  `BILLIARDS_HEADLESS=1` when the server announces it (new field next to
  `client_log_level`), so switching between windowed/headless needs no manual
  step on the laptop.

### dev-relay

- Serve config / MCP tool to toggle the mode (e.g. `client_headless` alongside
  `client_log_level`, same poll-and-restart mechanism).
- Sessions listing shows `headless=true` so logs/captures are attributable.

## Testing wins once landed

- The whole MCP debug loop (re-rack, injected shots, screenshots, ball
  positions, log assertions) can run unattended — e.g. as a smoke test after
  every build before the developer ever looks at it.
- Deterministic screenshots (no compositor/window-manager interference).
- No focus stealing during long automated runs.

## Open questions

- WGPU headless on the laptop's iGPU: need to confirm offscreen rendering
  works there (it should; it's the same adapter).
- egui dev panel in headless mode — render to texture or skip.
- Depth captures (`depth` MCP tool) need the depth pass preserved in
  render-to-texture mode.

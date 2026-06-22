# dynamicnoti

A fast, modular, **data-driven** desktop notification daemon for **Artix Linux + KDE Plasma 6 (Wayland/KWin 6.7)**, written in Rust. Renders a small top-center "Dynamic Island" black rounded rectangle with springy, highly-animated transitions. It **owns `org.freedesktop.Notifications`** (replacing KDE), watches **Cider via MPRIS** for rich song notifications, and accepts custom scripts over a Unix socket.

Current state and what's next live in **Build sequencing** at the bottom — the single source of truth for progress.

## Critical constraints (read before touching anything)

- **No systemd — this is OpenRC.** Never write `.service` files; they are silently ignored. Supervise via XDG autostart + a respawn wrapper (`dist/`). An OpenRC init script is the *headless-only* fallback.
- **wgpu + Wayland handles are `!Send`.** Everything in `dynamicnoti-render` is main-thread-pinned. Moving any of it into a tokio task won't compile — don't try via `Arc`.
- **Owning `org.freedesktop.Notifications` means a crash drops ALL notifications system-wide.** Fault isolation (`catch_unwind`) and supervision are not optional. Release profile is `panic = "unwind"` — keep it that way or the fences break.
- **Cider's MPRIS bus name churns every launch** (`org.mpris.MediaPlayer2.chromium.instanceNNNN`). Match by the `Identity` property + allowlist; re-enumerate on `NameOwnerChanged`. Never hardcode the bus name.

## Architecture: two threads, joined by channels

```
 MAIN THREAD ("the loop")                      WORKER THREAD (tokio runtime)
 ┌───────────────────────────────┐            ┌────────────────────────────────┐
 │ calloop + Wayland + wgpu       │  events    │ Source tasks:                  │
 │ glyphon atlas + image cache    │ ◀────────  │  - freedesktop (zbus server)   │
 │ spring state (dynamicnoti-anim)│ calloop::  │  - mpris (zbus client, Cider)  │
 │ render of dynamicnoti_core::   │  channel   │  - ipc (UnixListener)          │
 │   Scene                        │ ────────▶  │  + notify config watcher       │
 │   ALL !Send                    │  flume     │  ALL async / Send              │
 └───────────────────────────────┘ (signals)  └────────────────────────────────┘
```

- `main()` must **not** be `#[tokio::main]` — it stays free for calloop/wgpu. Spawn the runtime on a `std::thread`.
- Bridge: `calloop::channel::Sender<NotificationEvent>` (tokio→main, wakes the loop) and `flume` (main→tokio, for `NotificationClosed`/`ActionInvoked`).
- Decode album art on tokio (`spawn_blocking`); ship RGBA / a cache path through the channel. Only the GPU upload touches main.

## Crates (dependency direction is strict and acyclic)

| Crate | Role | May depend on |
|---|---|---|
| `dynamicnoti-core` | Domain model + the `scene` type system + config/theme/type loading. **Pure: no async/GPU/Wayland/zbus.** | — |
| `dynamicnoti-proto` | IPC wire types + socket framing. | — |
| `dynamicnoti-anim` | Spring physics (pure math, unit-tested). | — |
| `dynamicnoti-render` | SCTK layer-shell + wgpu + glyphon + Scene interpreter. **Main-thread only.** | core, anim |
| `dynamicnoti-sources` | `Source` trait + freedesktop/mpris/ipc (tokio). | core, proto |
| `dynamicnotid` (bin) | Daemon: thread split, channels, `catch_unwind` fences, supervision. | all libs |
| `dynamicnoti` (bin) | CLI. | **proto only** |

**The rule that makes it modular:** `core` emits `Scene` as plain data; `render` is the only crate that turns `Scene` into draw calls. Keep `core` free of I/O/GPU and keep the CLI on `proto` alone.

## Data-driven notification types (the core feature)

A notification **type** is a TOML file in `~/.config/dynamicnoti/types/<name>.toml`. **Adding a type = drop a `.toml`, no recompile.** Each type declares a field schema, a layout tree of primitives with bindings, an anim profile, style overrides, timeout, priority, and a `replace_key`.

- **Closed set of 6 primitives** (`crates/dynamicnoti-core/src/scene.rs`): `Text`, `Marquee`, `Image`, `Icon`, `Progress`, `Spacer`, arranged by `Row`/`Column`/`Stack`/`Leaf`. Extending this set is a deliberate cross-crate change; adding a *type* is not.
- **Bindings** (parsed in `scene.rs`; grammar also in `.claude/rules/notification-types.md`): a string with `{}` is `Format` (`"{artist} — {title}"`); a bare identifier is `FieldRef` (`art`, `status`; `Progress` reads `value = "position"`); a leading `=` (or a number/bool) is a baked `Literal` (`"=Sale ends soon"`).
- **Pipeline:** `RawNotification` → `TypeResolver` (explicit `type` hint → `SourceKind::default_type()`: mpris→`song`, freedesktop/ipc→`generic`) → `bind()` (validate/clamp — fault boundary #2) → `scene::build()` → immutable `Scene`.

## Animations

Per-property `Spring` (`crates/dynamicnoti-anim/src/lib.rs`), clamped semi-implicit Euler, substepped. Props: `width, height, scale, opacity, corner_radius, content_crossfade, marquee_offset`. Named presets in `theme.toml` (`island_soft`, `snappy`, `gentle`). Lifecycle (`Phase` in render): **Enter** (small+faint → measured size) → **Idle** (only marquee/progress) → **Morph** (crossfade out, swap Scene at midpoint, geometry-spring to new size — the signature island resize) → **Exit** (collapse, then destroy). **Drive ticks off Wayland frame callbacks; request the next frame only while `any_spring_unsettled || marquee_active`** — otherwise stop → 0% GPU.

## Commands

| Action | Command |
|---|---|
| Build all | `cargo build --workspace` |
| Test everything (no GPU needed) | `cargo test --workspace` |
| Test the pure crates only (fastest) | `cargo test -p dynamicnoti-anim -p dynamicnoti-core -p dynamicnoti-proto` |
| One test | `cargo test -p dynamicnoti-anim spring_settles_at_target` |
| Lint | `cargo clippy --workspace --all-targets` |
| Run daemon (real wgpu island) | `RUST_LOG=info cargo run -p dynamicnotid` |
| Run daemon headless (logs Scenes, no GPU) | `DYNAMICNOTI_HEADLESS=1 RUST_LOG=info cargo run -p dynamicnotid` |
| Build with D-Bus sources (steps 4–5) | `cargo build -p dynamicnoti-sources --features dbus` |
| Run CLI | `cargo run -p dynamicnoti -- --help` |
| Smoke the pipeline | `cargo run -p dynamicnoti -- post --type generic --field title=hi` |
| Who owns notifications | `busctl --user status org.freedesktop.Notifications` |
| List MPRIS players | `busctl --user list \| grep mpris` |
| Smoke a notification | `notify-send "hello" "world"` |

Before declaring work done, keep `cargo test --workspace` and `cargo clippy --workspace --all-targets` green on both default and `--features dbus`.

## Config (`~/.config/dynamicnoti/`)

`config.toml` (socket path, source allowlists, queue policy, monitor), `theme.toml` (colors, fonts, radius, blur, spring presets, anchor/size), `types/*.toml`. Seed copies live in `config.example/`. Live-reload via the `notify` crate on the tokio thread; a broken TOML logs and **keeps the last good config** — never crash on reload.

## Build sequencing (implement in this order)

Pinned, mutually-compatible set (source of truth: workspace `Cargo.toml`): wgpu 23 / glyphon 0.7 / calloop 0.13 / sctk 0.19.

1. ✅ `core` types + `anim` springs — pure, unit-tested, no compositor.
2. ✅ `proto` + CLI + `sources::ipc` (+ `core` resolve/bind/build, `QueueManager`, config watcher, daemon thread split) — UDS round-trip end-to-end, headless. Daemon runs `headless::run` (logs Scenes).
3. ✅ `render` — layer surface + wgpu (first `configure` acked before painting) → glyphon text → `Scene` interpreter (`layout.rs` is pure/tested) → springs on frame callbacks (`phase.rs` is pure/tested). Replaced `headless::run`; consumes `NotificationEvent::{Show,Morph,Close,ConfigReloaded,Shutdown}`, now carrying resolved `style`+`anim`. Includes Morph crossfade, marquee, album art, and `org_kde_kwin_blur`.
4. ✅ `sources::freedesktop` (behind `--features dbus`) — zbus server: `Notify`/`CloseNotification`/`GetCapabilities`/`GetServerInformation`, `NotificationClosed`/`ActionInvoked` signals, `NameLost` backoff. Name takeover gated by config `take_over` (**off by default**). The `flume` main→tokio return path is wired (render→driver→freedesktop). Each notification gets a `freedesktop:<id>` replace_key so closes route back to the right D-Bus id.
5. **← NEXT.** `sources::mpris` (behind `--features dbus`) — Cider `song` type, art fetch (decode on tokio, upload on main), suppress Cider's own freedesktop notifications. Seams ready: `image_cache` decodes from a path today; the `song` type + `mpris:single` replace_key already morph.
6. Reliability — all three fences live: #1 (ipc — tokio per-connection task isolation, a panicking handler dies with its task), #2 (bind/build `catch_unwind` in `driver.rs`), #3 (per-surface draw `catch_unwind` in `app::render_frame`). `dist/` supervise wrapper + autostart exist; live config reload (re-read + keep-last-good) done.

## Gotchas (these break silently)

- **Render before the first `configure` ack = blank surface / protocol error.** Layer surfaces start 0×0; size the wgpu surface only after acking configure.
- **Forget to request the next frame callback → frozen animation.** Conversely, requesting forever → 100% GPU. Gate on `any_spring_unsettled || marquee_active`.
- **`PresentMode::Fifo`** (vsync) only — don't add an independent animation timer that double-drives the loop.
- **Anchor TOP only** (centered island), `exclusive_zone = -1`, layer `OVERLAY`, `keyboard_interactivity = None` unless an action needs focus (grabbing keyboard steals it from the focused app).
- **Multi-GPU box (AMD dGPU + iGPU):** prefer a low-power adapter; a persistent dGPU context for a 400px overlay wastes power/heat.
- **`catch_unwind` needs `AssertUnwindSafe`** around wgpu/Wayland types; keep the caught region tiny (one notification's bind/draw) and re-validate state after.
- **KDE may reclaim the bus name** on Plasma restart. Disable Plasma's notification applet (see `.claude/skills/take-over-notifications.md`) and handle `NameLost` with backoff.

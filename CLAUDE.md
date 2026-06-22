# dynamicnoti

A fast, modular, **data-driven** desktop notification daemon for **Artix Linux + KDE Plasma 6 (Wayland/KWin 6.7)**, written in Rust. Renders a small top-center "Dynamic Island" black rounded rectangle with springy, highly-animated transitions. It **owns `org.freedesktop.Notifications`** (replacing KDE), watches **Cider via MPRIS** for rich song notifications, and accepts custom scripts over a Unix socket.

Steps 1–6 are implemented; the render/animation frontend and live MPRIS/Cider path are verified on real hardware and the frontend is ~80% polished. The only unverified surface left is the **intrusive** paths — the freedesktop bus takeover (replaces KDE) and supervised crash recovery. Remaining scope — finishing that verification, animation/design polish, and the config TUI — lives in **Build sequencing** at the bottom, the single source of truth for progress.

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

**Quality bar: the real iOS/macOS Dynamic Island** — every transition springy and overshooting, physically alive; never linear or eased tweens. This is the standard the polish pass (step 8) tunes toward.

Per-property `Spring` (`crates/dynamicnoti-anim/src/lib.rs`), clamped semi-implicit Euler, substepped. Props: `width, height, scale, opacity, corner_radius, crossfade, translate_y, marquee_offset`. Named presets in `theme.toml` (`island_soft`, `snappy`, `gentle`, `island_slide`). Lifecycle (`Phase` in render): **Enter** (small+faint, springs down from above via `translate_y`/`island_slide` → measured size at rest) → **Idle** (only marquee/progress) → **Morph** (crossfade out, swap Scene at midpoint, geometry-spring to new size — the signature island resize) → **Exit** (collapse + slide back up, then destroy). **Drive ticks off Wayland frame callbacks; request the next frame only while `any_spring_unsettled || marquee_active`** — otherwise stop → 0% GPU.

## Commands

| Action | Command |
|---|---|
| Build all | `cargo build --workspace` |
| Test everything (no GPU needed) | `cargo test --workspace` |
| Test the pure crates only (fastest) | `cargo test -p dynamicnoti-anim -p dynamicnoti-core -p dynamicnoti-proto` |
| One test | `cargo test -p dynamicnoti-anim spring_settles_at_target` |
| Lint | `cargo clippy --workspace --all-targets` |
| Run daemon (real wgpu island; D-Bus sources on by default) | `RUST_LOG=info cargo run -p dynamicnotid` |
| Run daemon headless (logs Scenes, no GPU) | `DYNAMICNOTI_HEADLESS=1 RUST_LOG=info cargo run -p dynamicnotid` |
| Build daemon without D-Bus (no zbus) | `cargo run -p dynamicnotid --no-default-features` |
| Run CLI | `cargo run -p dynamicnoti -- --help` |
| Smoke the pipeline | `cargo run -p dynamicnoti -- post --type generic --field title=hi` |
| Who owns notifications | `busctl --user status org.freedesktop.Notifications` |
| List MPRIS players | `busctl --user list \| grep mpris` |
| Smoke a notification | `notify-send "hello" "world"` |
| CLI: close / heartbeat | `cargo run -p dynamicnoti -- close --replace-key mpris:single` · `cargo run -p dynamicnoti -- ping` |
| CLI field syntax | `post [--type <name>] [--replace-key <k>] [--field k=v]…` — values infer float→bool→text; image fields are plain paths |
| Inspect socket / lock | `ls -l $XDG_RUNTIME_DIR/dynamicnoti.{sock,lock}` (socket gone ⇒ daemon down) |
| Install config seeds | `cp -rn config.example/* ~/.config/dynamicnoti/` |
| Install supervised autostart | `install -Dm755 dist/dynamicnotid-supervise ~/.local/bin/dynamicnotid-supervise && cp dist/dynamicnotid.desktop ~/.config/autostart/` |

Before declaring work done, keep `cargo test --workspace` and `cargo clippy --workspace --all-targets` green on both the default build (D-Bus sources on) and `--no-default-features` (headless-only, no zbus).

## Config (`~/.config/dynamicnoti/`)

`config.toml` (socket path, source allowlists, queue policy, monitor), `theme.toml` (colors, fonts, radius, blur, shadow, finish/sheen, spring presets, anchor/size), `types/*.toml`. **Working examples are the source of truth — read `config.example/{config.toml,theme.toml,types/*.toml}` before editing keys; the type-`.toml` grammar (primitives, bindings, layout tree) lives in `.claude/rules/notification-types.md`.** Live-reload via the `notify` crate on the tokio thread; a broken TOML logs and **keeps the last good config** — never crash on reload.

## Runtime environment & paths

Facts you can't infer at a glance — needed when the daemon won't start or the CLI can't reach it. Source: `dynamicnotid/src/main.rs`, `dynamicnoti-core/src/config.rs`, `dynamicnoti/src/main.rs`.

| Var / path | Default | Effect |
|---|---|---|
| `XDG_CONFIG_HOME` | else `$HOME/.config/dynamicnoti` | Config dir resolution order. |
| `XDG_RUNTIME_DIR` | else `/tmp` | Prefix for the socket `dynamicnoti.sock` and lock `dynamicnoti.lock`. |
| `dynamicnoti.lock` | in `$XDG_RUNTIME_DIR/` | `flock` single-instance guard; removed on clean exit. A stale lock blocks startup. |
| `RUST_LOG` | `dynamicnotid=info,dynamicnoti_sources=info` | `tracing` filter; logs to **stderr** (unbuffered). |
| `DYNAMICNOTI_HEADLESS` | unset | `=1` ⇒ GPU-free; logs Scenes instead of rendering (CI / pipeline debugging). |
| `DYNAMICNOTID_BIN` | `dynamicnotid` | Binary the `dist/` supervise wrapper respawns. |
| `MANGOHUD` / `DISABLE_MANGOHUD` | forced to 0 | Set before Vulkan init so the HUD overlay never paints on the island. |

## Key files (where the logic lives)

Landmarks for changing behavior — jump here instead of grepping. (Crate *roles* are in the Crates table above.)

| Concern | File | Note |
|---|---|---|
| Resolve type | `core/src/resolver.rs` | `type` hint → `SourceKind::default_type()`. |
| Validate/clamp fields | `core/src/bind.rs` | `bind()` — fault boundary #2. |
| Scene + primitives/bindings | `core/src/scene.rs` | `build()`, the 6 primitives, binding grammar. |
| Type schema | `core/src/template.rs` | `TypeTemplate`, `FieldSpec`. |
| Queue policy | `core/src/queue.rs` | `QueueManager` (priority-preempt / fifo / coalesce). |
| TUI schema feed | `core/src/introspect.rs` | `FieldMeta`/`FieldWidget` (step 9). |
| Render loop + fence #3 | `render/src/app.rs` | calloop + wgpu; per-surface draw `catch_unwind`. |
| Springs + lifecycle | `render/src/phase.rs` | Enter/Idle/Morph/Exit, per-property springs. |
| Layout (pure) | `render/src/layout.rs` | 2-pass measure/place; unit-tested. |
| GPU pipelines | `render/src/gpu.rs` | SDF rounded-rect + textured-quad. |
| Daemon wiring | `dynamicnotid/src/driver.rs` | thread split, fences #2/#3, source-task spawn. |
| Single-instance | `dynamicnotid/src/lock.rs` | `InstanceLock` (flock). |
| Sources | `sources/src/{freedesktop,mpris,ipc,watcher}.rs` | D-Bus server / MPRIS client / UDS / fs-watch. |

## Git / GitHub

- **Remote:** `origin` → `https://github.com/gainvel/dynamicnoti.git` (public, HTTPS); default branch `main`. `gh` CLI is installed.
- **Gitignored — never stage:** `/target`, `/.claude/`, `*.rs.bk`, `*.pdb`.
- **Flow:** `git status` → `git add -A` → `git commit` → `git push origin <branch>`; PRs via `gh pr create --fill`, status via `gh pr status` / `gh run list`.
- **Etiquette:** branch off `main` rather than committing to it directly, and push only when the user asks. Keep tests + clippy green on **both** the default build and `--no-default-features` before pushing (see Commands).

## Build sequencing (steps 1–6 landed — verification, polish, and the TUI remain)

Pinned, mutually-compatible set (source of truth: workspace `Cargo.toml`): wgpu 23 / glyphon 0.7 / calloop 0.13 / sctk 0.19.

1. ✅ `core` types + `anim` springs — pure, unit-tested, no compositor.
2. ✅ `proto` + CLI + `sources::ipc` (+ `core` resolve/bind/build, `QueueManager`, config watcher, daemon thread split) — UDS round-trip end-to-end, headless. Daemon runs `headless::run` (logs Scenes).
3. ✅ `render` — layer surface + wgpu (first `configure` acked before painting) → glyphon text → `Scene` interpreter (`layout.rs` is pure/tested) → springs on frame callbacks (`phase.rs` is pure/tested). Replaced `headless::run`; consumes `NotificationEvent::{Show,Morph,Close,ConfigReloaded,Shutdown}`, now carrying resolved `style`+`anim`. Includes Morph crossfade, marquee, album art, and `org_kde_kwin_blur`.
4. ✅ `sources::freedesktop` (behind `--features dbus`) — zbus server: `Notify`/`CloseNotification`/`GetCapabilities`/`GetServerInformation`, `NotificationClosed`/`ActionInvoked` signals, `NameLost` backoff. Name takeover gated by config `take_over` (**off by default**). The `flume` main→tokio return path is wired (render→driver→freedesktop). Each notification gets a `freedesktop:<id>` replace_key so closes route back to the right D-Bus id.
5. ✅ *(verified on live Cider)* `sources::mpris` (behind `--features dbus`) — Cider `song` type, art fetch (decode on tokio, upload on main), suppress Cider's own freedesktop notifications. Seams ready: `image_cache` decodes from a path today; the `song` type + `mpris:single` replace_key already morph.
6. 🔶 *(fences implemented; live crash recovery unverified — it's intrusive: a fault drops notifications system-wide)* Reliability — all three fences live: #1 (ipc — tokio per-connection task isolation, a panicking handler dies with its task), #2 (bind/build `catch_unwind` in `driver.rs`), #3 (per-surface draw `catch_unwind` in `app::render_frame`). `dist/` supervise wrapper + autostart exist; live config reload (re-read + keep-last-good) done.

**Remaining scope (the source of truth for what's next):**

7. **Verify the intrusive paths** — the render loop and live MPRIS/Cider are confirmed; what remains is the **freedesktop bus takeover from KDE** (replaces Plasma's notifications) and **supervised wgpu crash recovery**. Both are disruptive to exercise, so they're held until last; do them before relying on takeover in daily use.
8. 🔶 **Animation + design polish — ~80% done, the active focus.** Landed: springy slide-from-top (`translate_y`/`island_slide`), `finish` sheen, shadow tuning, `corner_radius` 12. Remaining are slight adjustments — keep tuning per-property springs (`dynamicnoti-anim`), the Morph crossfade, and `theme.toml` spacing/sizing to the quality bar in **Animations** above (the real Dynamic Island feel).
9. **Config TUI — the final step.** A feature-dense terminal control panel to edit `config.toml` / `theme.toml` / `types/*.toml` live. It consumes the machine-readable schema from `core::introspect` (`FieldMeta`/`FieldWidget`), so it renders a form for any user-defined type. Model it on the references: `storage-sorter-gui` (ratatui JSON-config form editor) and `gifscii` (live-preview tune loop). Plan: a new `dynamicnoti-tui` bin depending on `proto` + `core` only — same modularity rule as the CLI.

## Gotchas (these break silently)

- **Render before the first `configure` ack = blank surface / protocol error.** Layer surfaces start 0×0; size the wgpu surface only after acking configure.
- **Forget to request the next frame callback → frozen animation.** Conversely, requesting forever → 100% GPU. Gate on `any_spring_unsettled || marquee_active`.
- **`PresentMode::Fifo`** (vsync) only — don't add an independent animation timer that double-drives the loop.
- **Anchor TOP only** (centered island), `exclusive_zone = -1`, layer `OVERLAY`, `keyboard_interactivity = None` unless an action needs focus (grabbing keyboard steals it from the focused app).
- **Multi-GPU box (AMD dGPU + iGPU):** prefer a low-power adapter; a persistent dGPU context for a 400px overlay wastes power/heat.
- **`catch_unwind` needs `AssertUnwindSafe`** around wgpu/Wayland types; keep the caught region tiny (one notification's bind/draw) and re-validate state after.
- **KDE may reclaim the bus name** on Plasma restart. Disable Plasma's notification applet (see `.claude/skills/take-over-notifications.md`) and handle `NameLost` with backoff.

## Troubleshooting (runtime)

Symptom → likely cause → fix. (Build-time pitfalls are in **Gotchas** above.)

- **No notification appears at all.** dynamicnotid isn't the bus owner / `take_over = false` — check `busctl --user status org.freedesktop.Notifications`; to own it, follow `.claude/skills/take-over-notifications.md`.
- **KDE shows the notification instead.** Plasma's applet still owns (or reclaimed on restart) the name — disable it (skill above) and confirm `NameLost` backoff handled the swap.
- **Daemon exits immediately on launch.** Another instance holds `$XDG_RUNTIME_DIR/dynamicnoti.lock` (single-instance flock) — kill the stale process; the lock clears on clean exit.
- **CLI `post`/`close` errors `cannot connect`.** Socket missing ⇒ daemon down, or a different `XDG_RUNTIME_DIR` than the daemon's — verify `ls -l $XDG_RUNTIME_DIR/dynamicnoti.sock`.
- **Cider/MPRIS song never shows.** Identity not in the allowlist, or a churning bus name was hardcoded — check `[sources.mpris].identities` and `busctl --user list | grep mpris`.
- **Config edit had no effect.** Broken TOML ⇒ the daemon logged a parse error and **kept the last good config** — check stderr.
- **Blank island / frozen / 100% GPU.** Render-loop issues — see the `configure`-ack and frame-callback gotchas above.

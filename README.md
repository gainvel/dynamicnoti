# dynamicnoti

A fast, modular, data-driven desktop notification daemon for **KDE Plasma 6 on Wayland**
(Artix Linux). Notifications render as a small **Dynamic Island**-style black rounded rectangle
near the top-center of the screen, with springy, highly-animated transitions.

It **replaces KDE's notifications** (owns `org.freedesktop.Notifications`), shows rich
**now-playing cards for Cider** (via MPRIS), and lets **custom scripts** post their own typed
notifications (deals, price drops, news, anything) over a Unix socket.

## Why it's modular

- **Notification types are data.** A type is a TOML file in `~/.config/dynamicnoti/types/`
  composing a fixed set of render primitives (text, marquee, image, icon, progress, spacer).
  Add a new kind of notification by dropping in a `.toml` — no recompile.
- **The UI is fully themeable** — colors, fonts, radius, blur, position/size, and named spring
  presets in `theme.toml`.
- **Inputs are pluggable** — a freedesktop D-Bus server, an MPRIS watcher, and a JSON socket,
  all feeding one core pipeline.

## Workspace

```
crates/
  dynamicnoti-core      domain model + the data-driven scene/type system (pure)
  dynamicnoti-proto     IPC wire format (pure)
  dynamicnoti-anim      spring physics (pure)
  dynamicnoti-render    wlr-layer-shell + wgpu + glyphon UI (main-thread only)
  dynamicnoti-sources   freedesktop / mpris / ipc inputs (tokio)
  dynamicnotid          the daemon binary
  dynamicnoti           the CLI binary
config.example/         seed config.toml, theme.toml, types/*.toml
dist/                   autostart entry + respawn wrapper (supervision; no systemd here)
```

See **CLAUDE.md** for the architecture, threading model, gotchas, and the build sequencing the
implementation follows. Status: **headless backend complete** — the full data-driven pipeline
(resolve → bind → build → queue → event) runs end-to-end without a GPU; the daemon logs Scenes
via a headless renderer. The wgpu/wlr-layer-shell render loop and the D-Bus sources (freedesktop,
MPRIS) are next.

## Quick start (development)

```bash
cargo build --workspace
cargo test --workspace                          # all tests, no GPU needed
RUST_LOG=info cargo run -p dynamicnotid &        # daemon (headless: logs Scenes)
cargo run -p dynamicnoti -- post --type generic --field title="hello" --field body="world"
```

## Install (once implemented)

```bash
mkdir -p ~/.config/dynamicnoti && cp -rn config.example/* ~/.config/dynamicnoti/
install -Dm755 dist/dynamicnotid-supervise ~/.local/bin/dynamicnotid-supervise
cp dist/dynamicnotid.desktop ~/.config/autostart/
# then disable Plasma's own notifications: System Settings -> Notifications
```

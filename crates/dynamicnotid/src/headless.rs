//! Headless "renderer" — the GPU-free stand-in for `dynamicnoti_render::run`. It drains the
//! calloop channel and logs each scene, letting the whole backend run end-to-end without a
//! compositor. The per-event handler is wrapped in `catch_unwind` as the stand-in for
//! fault-isolation fence #3 (the real per-surface draw fence).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use calloop::channel::{Channel, Event};
use dynamicnoti_core::scene::{Primitive, Scene};
use dynamicnoti_render::NotificationEvent;

struct State {
    running: bool,
}

/// Run the headless event loop until a [`NotificationEvent::Shutdown`] arrives (or the channel
/// closes). Mirrors the contract of `dynamicnoti_render::run`: owns the main thread, returns
/// only on shutdown.
pub fn run(rx: Channel<NotificationEvent>) -> anyhow::Result<()> {
    let mut event_loop = calloop::EventLoop::<State>::try_new()?;
    event_loop
        .handle()
        .insert_source(rx, |event, _, state| match event {
            Event::Msg(ev) => {
                // Fence #3 stand-in: one bad event can't take down the loop.
                let _ = catch_unwind(AssertUnwindSafe(|| handle_event(ev, state)));
            }
            Event::Closed => state.running = false,
        })
        .map_err(|e| anyhow::anyhow!("failed to register render channel: {e}"))?;

    let mut state = State { running: true };
    tracing::info!(target: "headless", "headless renderer running (logs scenes; no GPU)");
    while state.running {
        event_loop.dispatch(Some(Duration::from_millis(250)), &mut state)?;
    }
    tracing::info!(target: "headless", "headless renderer stopped");
    Ok(())
}

fn handle_event(ev: NotificationEvent, state: &mut State) {
    match ev {
        NotificationEvent::Show { id, scene, .. } => {
            tracing::info!(target: "headless", "SHOW #{id}\n{}", dump(&scene));
        }
        NotificationEvent::Morph { id, scene, .. } => {
            tracing::info!(target: "headless", "MORPH #{id}\n{}", dump(&scene));
        }
        NotificationEvent::Close { id } => {
            tracing::info!(target: "headless", "CLOSE #{id}");
        }
        NotificationEvent::ImageReady { key, image } => {
            tracing::info!(
                target: "headless",
                "IMAGE READY {key:?} ({}x{})", image.width, image.height
            );
        }
        NotificationEvent::ConfigReloaded => {
            tracing::info!(target: "headless", "config reloaded");
        }
        NotificationEvent::Shutdown => {
            tracing::info!(target: "headless", "shutdown");
            state.running = false;
        }
    }
}

/// Render a `Scene` as an indented tree for the log.
fn dump(scene: &Scene) -> String {
    let mut out = String::new();
    dump_into(scene, 1, &mut out);
    out
}

fn dump_into(scene: &Scene, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match scene {
        Scene::Row { children, .. } => container(out, &indent, "Row", children, depth),
        Scene::Column { children, .. } => container(out, &indent, "Column", children, depth),
        Scene::Stack { children, .. } => container(out, &indent, "Stack", children, depth),
        Scene::Leaf(p) => {
            out.push_str(&indent);
            out.push_str(&primitive(p));
            out.push('\n');
        }
    }
}

fn container(out: &mut String, indent: &str, name: &str, children: &[Scene], depth: usize) {
    out.push_str(indent);
    out.push_str(name);
    out.push('\n');
    for c in children {
        dump_into(c, depth + 1, out);
    }
}

fn primitive(p: &Primitive) -> String {
    match p {
        Primitive::Text { content, .. } => format!("Text {content:?}"),
        Primitive::Marquee { content, speed_px_s, .. } => format!("Marquee {content:?} @{speed_px_s}px/s"),
        Primitive::Image { handle, radius } => format!("Image {handle:?} r={radius}"),
        Primitive::Icon { name, .. } => format!("Icon {name:?}"),
        Primitive::Progress { value, .. } => format!("Progress {value:.2}"),
        Primitive::Spacer { size } => format!("Spacer {size}"),
    }
}

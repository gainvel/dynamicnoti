//! KDE backdrop blur via `org_kde_kwin_blur` (the island gets the frosted-glass look). The
//! manager is an optional global: if KWin doesn't advertise it we degrade gracefully to a plain
//! translucent island. Region is a rectangle matching the surface; rounded corners still show a
//! faint square blur halo at the very corners — acceptable for v1.

use smithay_client_toolkit::compositor::{CompositorState, Region};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Dispatch, QueueHandle};
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

pub struct BlurManager {
    mgr: Option<OrgKdeKwinBlurManager>,
}

impl BlurManager {
    pub fn bind<D>(globals: &GlobalList, qh: &QueueHandle<D>) -> BlurManager
    where
        D: Dispatch<OrgKdeKwinBlurManager, ()> + 'static,
    {
        match globals.bind::<OrgKdeKwinBlurManager, _, _>(qh, 1..=1, ()) {
            Ok(mgr) => {
                tracing::info!(target: "render", "org_kde_kwin_blur available — backdrop blur on");
                BlurManager { mgr: Some(mgr) }
            }
            Err(_) => {
                tracing::info!(target: "render", "org_kde_kwin_blur unavailable — no backdrop blur");
                BlurManager { mgr: None }
            }
        }
    }

    pub fn available(&self) -> bool {
        self.mgr.is_some()
    }

    /// Attach a blur region `(0,0,w,h)` to `surface`. The returned blur object + region must
    /// outlive the next surface commit (store them on the island). A follow-up `commit` on the
    /// surface itself is required by the caller for the blur to take effect.
    pub fn apply<D>(
        &self,
        compositor: &CompositorState,
        qh: &QueueHandle<D>,
        surface: &WlSurface,
        w: i32,
        h: i32,
    ) -> Option<(OrgKdeKwinBlur, Region)>
    where
        D: Dispatch<OrgKdeKwinBlur, ()> + 'static,
    {
        let mgr = self.mgr.as_ref()?;
        let blur = mgr.create(surface, qh, ());
        let region = Region::new(compositor).ok()?;
        region.add(0, 0, w.max(1), h.max(1));
        blur.set_region(Some(region.wl_region()));
        blur.commit();
        Some((blur, region))
    }
}

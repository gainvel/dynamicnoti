//! Album-art / image cache: handle string → an uploaded texture bind group. Two entry points:
//! [`ImageCache::get`] decodes a local file/`file://` path on the main thread (freedesktop icons,
//! small art); [`ImageCache::insert_decoded`] uploads RGBA bytes a source already decoded off the
//! main thread (MPRIS remote album art) — there the GPU upload is the only main-thread work.

use std::collections::{HashMap, HashSet};

use dynamicnoti_core::ImageData;

use crate::gpu::Pipelines;

pub struct ImageCache {
    map: HashMap<String, wgpu::BindGroup>,
    failed: HashSet<String>,
}

impl ImageCache {
    pub fn new() -> ImageCache {
        ImageCache { map: HashMap::new(), failed: HashSet::new() }
    }

    /// Return a bind group for `handle`, decoding + uploading on first use. `None` if the file
    /// can't be loaded (the image leaf is then skipped — never a hard failure).
    pub fn get(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipes: &Pipelines,
        handle: &str,
    ) -> Option<&wgpu::BindGroup> {
        // Check the uploaded map FIRST: a handle may have been poisoned into `failed` by an
        // earlier `get` (e.g. an http URL won't `image::open`) and later filled by
        // `insert_decoded` once the source's async fetch landed.
        if self.map.contains_key(handle) {
            return self.map.get(handle);
        }
        if self.failed.contains(handle) {
            return None;
        }
        match load(device, queue, pipes, handle) {
            Some(bg) => {
                self.map.insert(handle.to_string(), bg);
            }
            None => {
                tracing::debug!(target: "render", "image load failed: {handle}");
                self.failed.insert(handle.to_string());
                return None;
            }
        }
        self.map.get(handle)
    }

    /// Upload already-decoded RGBA bytes (decoded off the main thread by a source) and cache the
    /// bind group under `key`. Clears any prior `failed` poisoning so a later [`get`] succeeds.
    pub fn insert_decoded(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipes: &Pipelines,
        key: &str,
        image: &ImageData,
    ) {
        let bg = upload_rgba(device, queue, pipes, image.width, image.height, &image.rgba);
        self.failed.remove(key);
        self.map.insert(key.to_string(), bg);
    }

    /// Immutable lookup of an already-loaded bind group (call after `get` has ensured the load).
    /// Lets the paint pass hold several image references at once without re-borrowing mutably.
    pub fn peek(&self, handle: &str) -> Option<&wgpu::BindGroup> {
        self.map.get(handle)
    }
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

fn load(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipes: &Pipelines,
    handle: &str,
) -> Option<wgpu::BindGroup> {
    // Accept a `file://` URI as well as a bare path (freedesktop/MPRIS art often uses URIs).
    let path = handle.strip_prefix("file://").unwrap_or(handle);
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    Some(upload_rgba(device, queue, pipes, w, h, &img))
}

/// Upload tightly-packed RGBA8 (`w*h*4` bytes) into an sRGB texture and build its bind group.
/// Shared by the on-main `load` decode path and the off-main `insert_decoded` path.
fn upload_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipes: &Pipelines,
    w: u32,
    h: u32,
    rgba: &[u8],
) -> wgpu::BindGroup {
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("album-art"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // sRGB so sampling decodes to linear, matching the linear premultiplied pipeline.
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(4 * w),
            rows_per_image: Some(h),
        },
        size,
    );

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("album-art-bg"),
        layout: pipes.image_bg_layout(),
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(pipes.sampler()) },
        ],
    })
}

//! Decoded, ready-to-upload image bytes. A plain-data handoff so a source (tokio) can download
//! and decode album art off the main thread and ship the raw RGBA to the renderer, whose only
//! job is the GPU upload. Pure: no GPU, no async — just `std`.

use std::sync::Arc;

/// One decoded image: tightly-packed RGBA8 (`width * height * 4` bytes, row-major, no padding).
/// `rgba` is an `Arc` so it can cross the source→driver→render channels without a deep copy.
#[derive(Clone, Debug)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

impl ImageData {
    /// Build from raw RGBA8 bytes. Returns `None` if the dimensions are zero or the buffer is
    /// the wrong length, so a malformed decode is dropped rather than panicking downstream.
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<ImageData> {
        if width == 0 || height == 0 {
            return None;
        }
        if rgba.len() != (width as usize) * (height as usize) * 4 {
            return None;
        }
        Some(ImageData { width, height, rgba: Arc::from(rgba) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_dims_and_wrong_len() {
        assert!(ImageData::from_rgba(0, 4, vec![0; 0]).is_none());
        assert!(ImageData::from_rgba(2, 2, vec![0; 8]).is_none(), "2x2 needs 16 bytes");
        let ok = ImageData::from_rgba(2, 2, vec![7; 16]).expect("valid");
        assert_eq!((ok.width, ok.height, ok.rgba.len()), (2, 2, 16));
    }
}

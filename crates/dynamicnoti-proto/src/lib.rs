//! dynamicnoti-proto — the IPC wire format between the `dynamicnoti` CLI (and custom user
//! scripts) and the `dynamicnotid` daemon. Deliberately depends on nothing internal.
//!
//! Transport: a Unix domain socket at `$XDG_RUNTIME_DIR/dynamicnoti.sock`, length-prefixed
//! JSON frames (u32 LE length, then that many bytes of JSON). A custom script can post a
//! notification with nothing but a socket and `serde_json` — no linking against this crate.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};

/// One wire value. Mirrors the renderable field kinds without importing core.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WireValue {
    Text(String),
    Float(f64),
    Bool(bool),
}

/// A request from a client to the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Post a notification. `type` selects the template (default "generic").
    Post {
        #[serde(rename = "type", default)]
        kind: Option<String>,
        #[serde(default)]
        replace_key: Option<String>,
        fields: HashMap<String, WireValue>,
    },
    /// Close a previously-posted notification by replace_key.
    Close { replace_key: String },
    /// Liveness check.
    Ping,
}

/// The daemon's reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok { id: u64 },
    Pong,
    Error { message: String },
}

/// Frame length prefix width, in bytes (u32 little-endian).
pub const LEN_PREFIX_BYTES: usize = 4;

/// Upper bound on a single frame's JSON body. A notification is tiny; anything larger is a
/// bug or a hostile client, so we reject it rather than allocate unboundedly.
pub const MAX_FRAME: usize = 1 << 20; // 1 MiB

/// Errors from the framing layer. Wire errors are kept separate from JSON/IO so callers can
/// distinguish a malformed peer from a dead socket.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame body of {0} bytes exceeds MAX_FRAME ({MAX_FRAME})")]
    TooLarge(usize),
    #[error("connection closed mid-frame")]
    Truncated,
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Serialize a message into a length-prefixed frame: `u32 LE length` + JSON body.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(LEN_PREFIX_BYTES + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parse one message from a complete frame buffer (`u32 LE length` + that many JSON bytes).
pub fn decode_frame<T: DeserializeOwned>(buf: &[u8]) -> Result<T, FrameError> {
    if buf.len() < LEN_PREFIX_BYTES {
        return Err(FrameError::Truncated);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let body = buf
        .get(LEN_PREFIX_BYTES..LEN_PREFIX_BYTES + len)
        .ok_or(FrameError::Truncated)?;
    Ok(serde_json::from_slice(body)?)
}

/// Blocking: write one frame to `w`. Used by the CLI; the daemon uses async helpers in
/// `dynamicnoti-sources` that reuse [`encode_frame`].
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    let frame = encode_frame(msg)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Blocking: read one frame from `r` and deserialize it.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    let mut len_buf = [0u8; LEN_PREFIX_BYTES];
    read_exact_eof(r, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    read_exact_eof(r, &mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

/// Like `Read::read_exact`, but maps an EOF at a frame boundary to [`FrameError::Truncated`].
fn read_exact_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), FrameError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(FrameError::Truncated),
        Err(e) => Err(FrameError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_request_roundtrips_json() {
        let mut fields = HashMap::new();
        fields.insert("title".to_string(), WireValue::Text("Price drop!".into()));
        let req = Request::Post { kind: Some("deal".into()), replace_key: None, fields };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Post { kind, .. } => assert_eq!(kind.as_deref(), Some("deal")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn frame_roundtrips_through_buffer() {
        let req = Request::Close { replace_key: "mpris:single".into() };
        let frame = encode_frame(&req).unwrap();
        // 4-byte prefix + body, and the prefix matches the body length.
        let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(len, frame.len() - LEN_PREFIX_BYTES);
        let back: Request = decode_frame(&frame).unwrap();
        matches!(back, Request::Close { .. });
    }

    #[test]
    fn blocking_read_write_roundtrip() {
        let resp = Response::Ok { id: 7 };
        let mut buf = Vec::new();
        write_frame(&mut buf, &resp).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let back: Response = read_frame(&mut cursor).unwrap();
        assert!(matches!(back, Response::Ok { id: 7 }));
    }

    #[test]
    fn oversized_length_prefix_is_rejected() {
        let mut frame = (MAX_FRAME as u32 + 1).to_le_bytes().to_vec();
        frame.extend_from_slice(b"{}");
        let err = decode_frame::<Request>(&frame).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }

    #[test]
    fn truncated_frame_is_detected() {
        // Claims 100 bytes but provides none.
        let frame = 100u32.to_le_bytes().to_vec();
        let err = decode_frame::<Request>(&frame).unwrap_err();
        assert!(matches!(err, FrameError::Truncated));

        // A short read on the blocking path also reports Truncated.
        let mut cursor = std::io::Cursor::new(frame);
        let err = read_frame::<_, Request>(&mut cursor).unwrap_err();
        assert!(matches!(err, FrameError::Truncated));
    }
}

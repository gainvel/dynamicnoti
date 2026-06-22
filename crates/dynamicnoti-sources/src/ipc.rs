//! IPC source — a tokio `UnixListener` speaking the length-prefixed JSON protocol from
//! `dynamicnoti-proto`. Custom scripts (or the `dynamicnoti` CLI) post typed notifications here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dynamicnoti_core::{RawNotification, SourceKind};
use dynamicnoti_proto::{decode_frame, encode_frame, FrameError, Request, Response, MAX_FRAME};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use crate::{wire_to_value, SourceMsg, SourceSender};

/// Bind the socket at `path` and serve clients until cancelled. The caller must already hold
/// the single-instance lock before calling this — we remove a stale socket file here.
pub async fn run(path: PathBuf, tx: SourceSender) -> anyhow::Result<()> {
    // A leftover socket from an unclean shutdown blocks bind(); safe to remove now that the
    // lock guarantees we're the only daemon.
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)
        .map_err(|e| anyhow::anyhow!("cannot bind IPC socket {path:?}: {e}"))?;
    restrict_permissions(&path);
    tracing::info!(target: "ipc", "listening on {path:?}");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_conn(stream, tx).await {
                        tracing::debug!(target: "ipc", "connection ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(target: "ipc", "accept failed: {e}");
            }
        }
    }
}

/// Handle one connection: read a request, act on it, write the response. A malformed frame
/// yields an `Error` response and closes the connection — it never propagates out to the
/// accept loop (fault-isolation boundary #1).
async fn serve_conn(mut stream: UnixStream, tx: SourceSender) -> anyhow::Result<()> {
    let req: Request = match read_frame(&mut stream).await {
        Ok(r) => r,
        Err(e) => {
            let _ = write_frame(&mut stream, &Response::Error { message: format!("{e}") }).await;
            return Ok(());
        }
    };

    let resp = match req {
        Request::Ping => Response::Pong,
        Request::Post { kind, replace_key, fields } => {
            let raw = RawNotification {
                source: SourceKind::Ipc,
                app_name: "ipc".into(),
                requested_type: kind,
                replace_key,
                fields: fields
                    .iter()
                    .map(|(k, v)| (k.clone(), wire_to_value(v)))
                    .collect::<HashMap<_, _>>(),
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx.send(SourceMsg::Post { raw, reply: Some(reply_tx) }).is_err() {
                Response::Error { message: "daemon pipeline closed".into() }
            } else {
                match reply_rx.await {
                    Ok(Ok(id)) => Response::Ok { id },
                    Ok(Err(message)) => Response::Error { message },
                    Err(_) => Response::Error { message: "no reply from pipeline".into() },
                }
            }
        }
        Request::Close { replace_key } => {
            if tx.send(SourceMsg::Close { replace_key }).is_err() {
                Response::Error { message: "daemon pipeline closed".into() }
            } else {
                Response::Ok { id: 0 }
            }
        }
    };

    write_frame(&mut stream, &resp).await?;
    Ok(())
}

fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// Async read of one length-prefixed frame, reusing the pure codec from `dynamicnoti-proto`.
pub async fn read_frame<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, FrameError> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(map_eof)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.map_err(map_eof)?;
    // Re-prefix so we can hand the whole frame to the shared decoder.
    let mut frame = len_buf.to_vec();
    frame.extend_from_slice(&body);
    decode_frame(&frame)
}

/// Async write of one frame.
pub async fn write_frame<T: serde::Serialize>(
    stream: &mut UnixStream,
    msg: &T,
) -> Result<(), FrameError> {
    let frame = encode_frame(msg)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

fn map_eof(e: std::io::Error) -> FrameError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        FrameError::Truncated
    } else {
        FrameError::Io(e)
    }
}

//! End-to-end socket test: the real IPC listener, a real client connection, real frames.

use std::time::Duration;

use dynamicnoti_proto::{Request, Response};
use dynamicnoti_sources::{ipc, SourceMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

async fn write_frame(stream: &mut UnixStream, req: &Request) {
    let frame = dynamicnoti_proto::encode_frame(req).unwrap();
    stream.write_all(&frame).await.unwrap();
    stream.flush().await.unwrap();
}

async fn read_response(stream: &mut UnixStream) -> Response {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await.unwrap();
    let n = u32::from_le_bytes(len) as usize;
    let mut body = vec![0u8; n];
    stream.read_exact(&mut body).await.unwrap();
    let mut frame = len.to_vec();
    frame.extend_from_slice(&body);
    dynamicnoti_proto::decode_frame(&frame).unwrap()
}

#[tokio::test]
async fn ping_pongs_and_post_gets_id() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("test.sock");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SourceMsg>();

    // Stand in for the driver: assign id 42 to any Post.
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let SourceMsg::Post { reply: Some(reply), .. } = msg {
                let _ = reply.send(Ok(42));
            }
        }
    });

    let listener_socket = socket.clone();
    tokio::spawn(async move {
        let _ = ipc::run(listener_socket, tx).await;
    });

    // Wait for the socket to appear.
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Ping → Pong.
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    write_frame(&mut stream, &Request::Ping).await;
    assert!(matches!(read_response(&mut stream).await, Response::Pong));

    // Post → Ok{ id: 42 } (the stand-in driver's assigned id).
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    write_frame(
        &mut stream,
        &Request::Post { kind: Some("generic".into()), replace_key: None, fields: Default::default() },
    )
    .await;
    assert!(matches!(read_response(&mut stream).await, Response::Ok { id: 42 }));
}

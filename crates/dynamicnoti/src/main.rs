//! dynamicnoti — the CLI. Posts notifications to the running daemon over the IPC socket.
//! Depends ONLY on dynamicnoti-proto (keep it that way — no wgpu/zbus/tokio here).
//!
//! Usage:
//!   dynamicnoti post --type deal --field title="Price drop" --field body="$19.99"
//!   dynamicnoti post --field title="hi"            # defaults to the "generic" type
//!   dynamicnoti close --replace-key mpris:single
//!   dynamicnoti ping
//! Custom scripts can skip this binary entirely and write length-prefixed JSON to the socket.

use dynamicnoti_proto::{Request, Response, WireValue};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some("ping") => send_and_report(Request::Ping),
        Some("post") => {
            let req = parse_post(&args[1..])?;
            send_and_report(req)
        }
        Some("close") => {
            let req = parse_close(&args[1..])?;
            send_and_report(req)
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

/// Parse `post [--type <name>] [--replace-key <k>] [--field k=v]...`.
fn parse_post(args: &[String]) -> anyhow::Result<Request> {
    let mut kind: Option<String> = None;
    let mut replace_key: Option<String> = None;
    let mut fields: HashMap<String, WireValue> = HashMap::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--type" => kind = Some(next_value(&mut it, "--type")?),
            "--replace-key" => replace_key = Some(next_value(&mut it, "--replace-key")?),
            "--field" => {
                let kv = next_value(&mut it, "--field")?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--field expects k=v, got '{kv}'"))?;
                fields.insert(k.to_string(), infer_value(v));
            }
            other => anyhow::bail!("unexpected argument to post: '{other}'"),
        }
    }

    Ok(Request::Post { kind, replace_key, fields })
}

/// Parse `close --replace-key <k>`.
fn parse_close(args: &[String]) -> anyhow::Result<Request> {
    let mut replace_key: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--replace-key" => replace_key = Some(next_value(&mut it, "--replace-key")?),
            other => anyhow::bail!("unexpected argument to close: '{other}'"),
        }
    }
    let replace_key = replace_key.ok_or_else(|| anyhow::anyhow!("close requires --replace-key"))?;
    Ok(Request::Close { replace_key })
}

fn next_value<'a>(
    it: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> anyhow::Result<String> {
    it.next()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

/// Infer a wire type from an unquoted CLI value: number, then bool, else text. Image fields
/// are passed as plain paths (text); the daemon's schema coerces them to images.
fn infer_value(raw: &str) -> WireValue {
    if let Ok(f) = raw.parse::<f64>() {
        return WireValue::Float(f);
    }
    match raw {
        "true" => WireValue::Bool(true),
        "false" => WireValue::Bool(false),
        _ => WireValue::Text(raw.to_string()),
    }
}

/// The socket path: `$XDG_RUNTIME_DIR/dynamicnoti.sock`, falling back to `/tmp` when the
/// runtime dir is unset (e.g. a bare shell).
fn socket_path() -> String {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    format!("{base}/dynamicnoti.sock")
}

fn send_and_report(req: Request) -> anyhow::Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| anyhow::anyhow!("cannot connect to dynamicnotid at {path}: {e}"))?;
    dynamicnoti_proto::write_frame(&mut stream, &req)?;
    let resp: Response = dynamicnoti_proto::read_frame(&mut stream)?;
    match resp {
        Response::Ok { id } => {
            println!("ok (id {id})");
            Ok(())
        }
        Response::Pong => {
            println!("pong");
            Ok(())
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!(
        "dynamicnoti — post notifications to dynamicnotid\n\n\
         USAGE:\n  dynamicnoti post [--type <name>] [--replace-key <k>] [--field k=v]...\n  \
         dynamicnoti close --replace-key <k>\n  dynamicnoti ping\n"
    );
}

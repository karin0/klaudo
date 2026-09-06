//! Drives the hook chain end to end against a server that answers like Telegram, so the
//! calls a turn makes, their order and the notification each carries are checked without
//! a network or a chat.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::json;

const PATIENCE: Duration = Duration::from_secs(20);

#[test]
fn a_turn_keeps_a_message_per_segment_and_rings_once_at_the_end() {
    let session = format!("test-{}", std::process::id());
    let (port, calls) = recorder();
    let root = prepare(&session);
    let socket = root.join("run/klaude").join(format!("{session}.sock"));

    hook(
        &root,
        port,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "cwd": env!("CARGO_MANIFEST_DIR"),
        }),
    );
    wait_for(|| socket.exists(), "the daemon never bound its socket");

    for (message, index, delta) in [
        ("m1", 0, "first "),
        ("m1", 1, "segment "),
        ("m1", 2, "text"),
        ("m2", 0, "second segment"),
    ] {
        hook(
            &root,
            port,
            &json!({
                "hook_event_name": "MessageDisplay",
                "session_id": session,
                "cwd": env!("CARGO_MANIFEST_DIR"),
                "message_id": message,
                "index": index,
                "delta": delta,
            }),
        );
        std::thread::sleep(Duration::from_millis(400));
    }

    hook(
        &root,
        port,
        &json!({
            "hook_event_name": "Stop",
            "session_id": session,
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "last_assistant_message": "second segment",
        }),
    );

    let made = collect(&calls, "sendRichMessage ring");
    assert_eq!(
        made,
        [
            // The first segment streams into a draft of its own.
            "sendRichMessageDraft",
            // The second segment starting is what tells the first one it is complete.
            "sendRichMessage silent",
            "sendRichMessageDraft",
            // The last segment is not posted on its own; Stop carries its text, with the
            // elapsed time, and makes the one sound of the turn.
            "sendRichMessage ring",
        ]
    );
    wait_for(
        || !socket.exists(),
        "the daemon outlived the turn it was reporting",
    );
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// Consecutive repeats collapse, because how many frames a draft takes is a matter of
/// how fast the deltas arrived.
fn collect(calls: &Receiver<String>, last: &str) -> Vec<String> {
    let deadline = Instant::now() + PATIENCE;
    let mut made: Vec<String> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let call = calls
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("the turn stopped after {made:?}"));
        let done = call == last;
        if made.last() != Some(&call) {
            made.push(call);
        }
        if done {
            return made;
        }
    }
}

fn prepare(session: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("klaude-{session}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("run")).expect("runtime directory");
    root
}

/// The credentials reach the daemon the way they reach it in a hook: through the
/// environment the command was started with.
fn hook(root: &Path, port: u16, event: &serde_json::Value) {
    let mut client = Command::new(env!("CARGO_BIN_EXE_klaude"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("BOT_TOKEN", "111111:secret")
        .env("CHAT_ID", "1")
        .env("API_BASE", format!("http://127.0.0.1:{port}"))
        .stdin(Stdio::piped())
        .spawn()
        .expect("run the hook");
    client
        .stdin
        .take()
        .expect("stdin")
        .write_all(event.to_string().as_bytes())
        .expect("write the event");
    assert!(client.wait().expect("wait").success(), "the hook failed");
}

fn wait_for(ready: impl Fn() -> bool, complaint: &str) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{complaint}");
}

fn recorder() -> (u16, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        for (id, stream) in listener.incoming().enumerate() {
            answer(stream.expect("accept"), id + 1, &sender);
        }
    });
    (port, receiver)
}

fn answer(mut stream: TcpStream, id: usize, calls: &Sender<String>) {
    let mut request = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    request.read_line(&mut line).expect("request line");
    let method = line
        .split_whitespace()
        .nth(1)
        .and_then(|path| path.rsplit('/').next())
        .expect("method")
        .to_owned();

    let mut length = 0;
    loop {
        let mut header = String::new();
        request.read_line(&mut header).expect("header");
        if header.trim().is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().expect("content length");
        }
    }
    let mut body = vec![0; length];
    request.read_exact(&mut body).expect("body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("a JSON body");

    let sound = match method.as_str() {
        "sendRichMessage" if body["disable_notification"] == json!(true) => " silent",
        "sendRichMessage" => " ring",
        _ => "",
    };
    calls.send(format!("{method}{sound}")).expect("record");

    let sent = json!({"ok": true, "result": {"message_id": id}}).to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{sent}",
        sent.len()
    )
    .expect("answer");
}

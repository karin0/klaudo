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
        made.iter()
            .map(|call| call.label.as_str())
            .collect::<Vec<_>>(),
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
    frame(&made[0].markdown, "first segment text");
    wait_for(
        || !socket.exists(),
        "the daemon outlived the turn it was reporting",
    );
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// One call the daemon made, as the server saw it.
struct Call {
    label: String,
    markdown: String,
}

/// Consecutive repeats collapse, because how many frames a draft takes is a matter of
/// how fast the deltas arrived. The body kept is the last of them, so a draft reads as
/// the frame its segment ended on.
fn collect(calls: &Receiver<Call>, last: &str) -> Vec<Call> {
    let deadline = Instant::now() + PATIENCE;
    let mut made: Vec<Call> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let call = calls.recv_timeout(left).unwrap_or_else(|_| {
            panic!(
                "the turn stopped after {:?}",
                made.iter().map(|call| &call.label).collect::<Vec<_>>()
            )
        });
        let done = call.label == last;
        match made.last_mut() {
            Some(kept) if kept.label == call.label => kept.markdown = call.markdown,
            _ => made.push(call),
        }
        if done {
            return made;
        }
    }
}

/// A frame opens with the head, carries the text streamed so far, and closes with the
/// status line: a mark, a word and the turn's elapsed time, inside the tag Telegram
/// animates.
fn frame(markdown: &str, text: &str) {
    let (title, body) = markdown.split_once("\n\n").expect("a head and a body");
    assert!(title.starts_with("**klaude**"), "head reads {title:?}");
    let (streamed, status) = body.split_once('\n').expect("text and a status line");
    assert_eq!(streamed, text);
    let status = status
        .strip_prefix("<tg-thinking>")
        .and_then(|line| line.strip_suffix("</tg-thinking>"))
        .expect("the animated tag");
    let status = status.strip_prefix("✻ ").expect("the mark");
    let (word, took) = status.split_once("… ").expect("a word and an elapsed time");
    assert!(
        !word.is_empty() && word.chars().all(char::is_alphabetic),
        "status word reads {word:?}"
    );
    assert!(
        took.starts_with('(') && took.ends_with("s)"),
        "elapsed time reads {took:?}"
    );
    println!("frame:\n{markdown}");
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

fn recorder() -> (u16, Receiver<Call>) {
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

fn answer(mut stream: TcpStream, id: usize, calls: &Sender<Call>) {
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
    let markdown = body["rich_message"]["markdown"]
        .as_str()
        .expect("a markdown body")
        .to_owned();
    calls
        .send(Call {
            label: format!("{method}{sound}"),
            markdown,
        })
        .expect("record");

    let sent = json!({"ok": true, "result": {"message_id": id}}).to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{sent}",
        sent.len()
    )
    .expect("answer");
}

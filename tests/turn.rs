//! Drives the hook chain end to end against a server that answers like Telegram, so the
//! calls a turn makes, their order, what each replies to and the notification each
//! carries are checked without a network or a chat.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

const PATIENCE: Duration = Duration::from_secs(20);

#[test]
fn a_turn_posts_the_prompt_and_replies_to_it_once_per_segment() {
    let (port, calls, _chat) = recorder();
    let root = prepare("segments");
    let resident = resident(&root, port);
    let session = "0123456789abcdef";

    hook(
        &root,
        port,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "prompt": "what does it do",
        }),
    );

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
            "last_assistant_message": "second segment",
        }),
    );

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        made.iter()
            .map(|call| call.label.as_str())
            .collect::<Vec<_>>(),
        [
            // The turn opens with what was asked, which the rest of it replies to.
            "sendRichMessage silent",
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
    assert_eq!(
        made[0].markdown,
        "**klaude** `01234567`\n\n>what does it do"
    );
    assert_eq!(made[0].reply, json!(null), "the prompt opens the thread");
    frame(&made[1].markdown, "first segment text");
    // Nothing retires a draft, so the last frame is the answer rather than whatever the
    // turn happened to be doing when it ended.
    assert_eq!(
        made[3].markdown, made[4].markdown,
        "the last frame leaves the answer on screen"
    );
    // A prompt deleted from the chat leaves the answer to it a message of its own.
    let reply = replying_to(made[0].id);
    assert_eq!(made[2].reply, reply, "the first segment replies");
    assert_eq!(made[4].reply, reply, "the last message replies");
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A prompt submitted while a turn is running is queued by Claude Code and reported
/// under the running turn's id, so the turn it eventually gets is announced by the
/// first event carrying an id of its own. Each turn must still answer its own prompt.
#[test]
fn a_prompt_queued_during_a_turn_gets_a_thread_of_its_own() {
    let (port, calls, _chat) = recorder();
    let root = prepare("queue");
    let resident = resident(&root, port);
    let session = "fedcba9876543210";
    let first = "aaaaaaaa-1111";
    let second = "bbbbbbbb-2222";

    // "second ask" is submitted while the first turn is running, which is why Claude
    // Code reports it under that turn's id. The queued turn's own id appears with its
    // first delta, and that is what has to open a thread of its own.
    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "first ask", "prompt_id": first}),
        json!({"hook_event_name": "MessageDisplay", "prompt_id": first, "message_id": "m1", "index": 0, "delta": "one"}),
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "second ask", "prompt_id": first}),
        json!({"hook_event_name": "Stop", "prompt_id": first, "last_assistant_message": "one"}),
        json!({"hook_event_name": "MessageDisplay", "prompt_id": second, "message_id": "m2", "index": 0, "delta": "two"}),
        json!({"hook_event_name": "Stop", "prompt_id": second, "last_assistant_message": "two"}),
    ];
    for mut event in turn {
        event["session_id"] = json!(session);
        event["cwd"] = json!(env!("CARGO_MANIFEST_DIR"));
        let streaming = event["hook_event_name"] == json!("MessageDisplay");
        hook(&root, port, &event);
        if streaming {
            // Long enough for the frame a segment's first delta puts on screen.
            std::thread::sleep(Duration::from_millis(400));
        }
    }

    let made = collect(&calls, |call| {
        call.label == "sendRichMessage ring" && call.markdown.ends_with("\n\ntwo")
    });
    assert_eq!(
        made.iter()
            .map(|call| call.label.as_str())
            .collect::<Vec<_>>(),
        [
            "sendRichMessage silent", // first ask
            "sendRichMessageDraft",   // the first turn streaming
            "sendRichMessage silent", // second ask, queued
            "sendRichMessageDraft",   // the first turn's answer, framed before it is sent
            "sendRichMessage ring",   // the first turn's answer
            "sendRichMessageDraft",   // the queued turn streaming, then its answer framed
            "sendRichMessage ring",   // the queued turn's answer
        ]
    );
    // A queued prompt has no turn yet, so its head addresses the session alone.
    assert_eq!(made[2].markdown, "**klaude** `fedcba98`\n\n>second ask");
    assert_eq!(
        made[4].reply,
        replying_to(made[0].id),
        "the first answer replies to the first ask"
    );
    assert_eq!(
        made[6].reply,
        replying_to(made[2].id),
        "the queued turn replies to the prompt that was queued"
    );
    assert!(
        made[6]
            .markdown
            .starts_with("**klaude** `fedcba98/bbbbbbbb`"),
        "the queued turn's head reads {:?}",
        made[6].markdown
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A turn that has said nothing is already on screen, so the minutes it spends thinking
/// or in tool calls read as the status line's own clock.
#[test]
fn a_turn_is_on_screen_before_it_has_said_anything() {
    let (port, calls, _chat) = recorder();
    let root = prepare("waiting");
    let resident = resident(&root, port);

    hook(
        &root,
        port,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "0123456789abcdef",
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "prompt": "what does it do",
        }),
    );

    let made = collect(&calls, |call| call.label == "sendRichMessageDraft");
    frame(&made.last().expect("a frame").markdown, "");
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A turn that talked, worked and talked again leaves three messages in order, and the
/// run of tool calls is the middle one.
#[test]
fn a_run_of_tool_calls_is_a_message_of_its_own() {
    let (port, calls, _chat) = recorder();
    let root = prepare("tools");
    let resident = resident(&root, port);
    let session = "0123456789abcdef";

    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "run the tests"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0, "delta": "on it"}),
        json!({"hook_event_name": "PreToolUse", "tool_use_id": "t1", "tool_name": "Bash",
               "tool_input": {"command": "cargo test"}}),
        json!({"hook_event_name": "PreToolUse", "tool_use_id": "t2", "tool_name": "Read",
               "tool_input": {"file_path": "/src/listen.rs"}, "agent_type": "Explore"}),
        json!({"hook_event_name": "PostToolUse", "tool_use_id": "t2", "tool_name": "Read",
               "duration_ms": 12}),
        json!({"hook_event_name": "PostToolUseFailure", "tool_use_id": "t1", "tool_name": "Bash",
               "duration_ms": 4187, "error": "Exit code 1\nassertion failed"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m2", "index": 0, "delta": "one test fails"}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "one test fails"}),
    ];
    for mut event in turn {
        event["session_id"] = json!(session);
        event["cwd"] = json!(env!("CARGO_MANIFEST_DIR"));
        let streaming = event["hook_event_name"] == json!("MessageDisplay");
        hook(&root, port, &event);
        if streaming {
            std::thread::sleep(Duration::from_millis(400));
        }
    }

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        made.iter()
            .map(|call| call.label.as_str())
            .collect::<Vec<_>>(),
        [
            "sendRichMessage silent", // the prompt
            "sendRichMessageDraft",   // what the turn said first
            "sendRichMessage silent", // posted when the first tool call opens the run
            "sendRichMessageDraft",   // the run, growing as its calls report
            "sendRichMessage silent", // posted when the turn talks again
            "sendRichMessageDraft",   // the last segment streaming
            "sendRichMessage ring",   // the answer
        ]
    );
    assert!(
        made[2].markdown.ends_with("\n\non it"),
        "what the turn said first reads {:?}",
        made[2].markdown
    );
    let (head, body) = made[4]
        .markdown
        .split_once("\n\n")
        .expect("a head and a run");
    assert!(
        head.starts_with("**klaude** `01234567/"),
        "head reads {head:?}"
    );
    assert_eq!(
        body,
        "× Bash `cargo test` 4s  \n⎿ Exit code 1  \n● [Explore] Read `/src/listen.rs` 12ms"
    );
    assert_eq!(
        made[4].reply,
        replying_to(made[0].id),
        "the run threads under the prompt"
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// The three hook processes run at once, so a delta can land after the `Stop` of its own
/// turn. Opening a second turn for it would leave a draft beside the answer showing
/// something else, and carrying the status line under it.
#[test]
fn a_delta_landing_after_its_stop_opens_no_second_turn() {
    let (port, calls, _chat) = recorder();
    let root = prepare("straggler");
    let resident = resident(&root, port);
    let session = "0123456789abcdef";
    let prompt = "aaaaaaaa-1111";

    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "say it", "prompt_id": prompt}),
        json!({"hook_event_name": "MessageDisplay", "prompt_id": prompt, "message_id": "m1", "index": 0, "delta": "said"}),
        json!({"hook_event_name": "Stop", "prompt_id": prompt, "last_assistant_message": "said it"}),
        json!({"hook_event_name": "MessageDisplay", "prompt_id": prompt, "message_id": "m1", "index": 1, "delta": " it"}),
    ];
    for mut event in turn {
        event["session_id"] = json!(session);
        event["cwd"] = json!(env!("CARGO_MANIFEST_DIR"));
        hook(&root, port, &event);
        std::thread::sleep(Duration::from_millis(400));
    }

    // Long enough for a frame of the turn the straggler would have opened.
    std::thread::sleep(Duration::from_millis(600));
    let made = drained(&calls);
    assert_eq!(
        made.last().map(|call| call.label.as_str()),
        Some("sendRichMessage ring"),
        "the answer is the last thing the turn does, not a draft after it"
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A message that replies to nothing still names a session: the one heard from last.
/// These sessions run outside tmux, so what klaude says back is where the message went.
#[test]
fn a_message_replying_to_nothing_goes_to_the_session_heard_from_last() {
    let (port, calls, chat) = recorder();
    let root = prepare("unaddressed");
    let resident = resident(&root, port);

    // The later session sorts first, so what answers is the one heard from last
    // rather than the first one klaude happens to hold.
    for session in ["fedcba9876543210", "0123456789abcdef"] {
        hook(
            &root,
            port,
            &json!({
                "hook_event_name": "SessionStart",
                "session_id": session,
                "cwd": env!("CARGO_MANIFEST_DIR"),
            }),
        );
    }
    chat.says("carry on");

    let made = collect(&calls, |call| {
        call.markdown.contains("is not running in tmux")
    });
    assert_eq!(
        made.last().expect("an answer").markdown,
        "`01234567` is not running in tmux"
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

fn replying_to(message_id: i64) -> serde_json::Value {
    json!({"message_id": message_id, "allow_sending_without_reply": true})
}

/// One call the resident made, as the server saw it.
struct Call {
    label: String,
    markdown: String,
    reply: serde_json::Value,
    /// What the server answered with, which is what a later message replies to.
    id: i64,
}

/// Every call the server has taken so far, in order, for a test that asserts what did
/// not happen and so cannot wait for a call to arrive.
fn drained(calls: &Receiver<Call>) -> Vec<Call> {
    calls.try_iter().collect()
}

/// Consecutive repeats collapse, because how many frames a draft takes is a matter of
/// how fast the deltas arrived. The body kept is the last of them, so a draft reads as
/// the frame its segment ended on.
fn collect(calls: &Receiver<Call>, done: impl Fn(&Call) -> bool) -> Vec<Call> {
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
        let last = done(&call);
        match made.last_mut() {
            Some(kept) if kept.label == call.label => *kept = call,
            _ => made.push(call),
        }
        if last {
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

fn prepare(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("klaude-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("run")).expect("runtime directory");
    root
}

/// The resident, killed when the test drops it.
struct Resident(Child);

impl Drop for Resident {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn resident(root: &Path, port: u16) -> Resident {
    let child = klaude(root, port)
        .arg("listen")
        .spawn()
        .expect("run the resident");
    let socket = root.join("run/klaude/listen.sock");
    wait_for(|| socket.exists(), "the resident never bound its socket");
    Resident(child)
}

/// The credentials reach the resident the way they reach it in a hook: through the
/// environment the command was started with.
fn klaude(root: &Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_klaude"));
    command
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("BOT_TOKEN", "111111:secret")
        .env("CHAT_ID", "1")
        .env("API_BASE", format!("http://127.0.0.1:{port}"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::piped());
    command
}

fn hook(root: &Path, port: u16, event: &serde_json::Value) {
    let mut client = klaude(root, port).spawn().expect("run the hook");
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

fn recorder() -> (u16, Receiver<Call>, Chat) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let (sender, receiver) = channel();
    let chat = Chat::default();
    let sending = chat.clone();
    std::thread::spawn(move || {
        let mut id = 0;
        for stream in listener.incoming() {
            id += 1;
            answer(stream.expect("accept"), id, &sender, &sending);
        }
    });
    (port, receiver, chat)
}

/// What the chat has to say, which the poll for updates hands over once.
#[derive(Clone, Default)]
struct Chat(Arc<Mutex<Vec<serde_json::Value>>>);

impl Chat {
    /// A message from the phone, as Telegram delivers it. It carries no
    /// `reply_to_message`, which is what makes it a message to route by itself.
    fn says(&self, text: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_secs();
        self.0.lock().expect("the chat").push(json!({
            "update_id": 1,
            "message": {
                "message_id": 9000,
                "date": now,
                "chat": {"id": 1},
                "from": {"id": 1},
                "text": text,
            },
        }));
    }

    fn drain(&self) -> Vec<serde_json::Value> {
        std::mem::take(&mut *self.0.lock().expect("the chat"))
    }
}

fn answer(mut stream: TcpStream, id: i64, calls: &Sender<Call>, chat: &Chat) {
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

    // The poll that asks what the chat said is not a call the turn made.
    let sent = if method == "getUpdates" {
        json!({"ok": true, "result": chat.drain()}).to_string()
    } else {
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
                reply: body["reply_parameters"].clone(),
                id,
            })
            .expect("record");
        json!({"ok": true, "result": {"message_id": id}}).to_string()
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{sent}",
        sent.len()
    )
    .expect("answer");
}

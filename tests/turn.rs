//! Drives the hook chain end to end against a server that answers like Telegram, so the
//! calls a turn makes, their order, what each replies to and the notification each
//! carries are checked without a network or a chat.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

const PATIENCE: Duration = Duration::from_secs(20);
/// How long a test waits for what follows the call it was watching for, so a rewrite or
/// a deletion issued right after the answer is part of what it reads.
const GRACE: Duration = Duration::from_millis(600);

#[test]
fn a_turn_posts_the_prompt_and_replies_to_it_once_per_segment() {
    let (port, calls, _chat) = recorder();
    let root = prepare("segments", port);
    let resident = resident(&root);
    let session = "0123456789abcdef";

    hook(
        &root,
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
        &json!({
            "hook_event_name": "Stop",
            "session_id": session,
            "last_assistant_message": "second segment",
        }),
    );

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        holding(&made),
        [
            // The turn opens with what was asked, which the rest of it replies to.
            ("silent", ">what does it do".to_owned()),
            // The second segment starting is what tells the first one it is complete.
            ("silent", "first segment text".to_owned()),
            // Stop carries the last segment's text and makes the one sound of the turn.
            ("ring", "second segment".to_owned()),
        ]
    );
    let sent: Vec<&Call> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage "))
        .collect();
    assert_eq!(
        sent[0].markdown,
        "**klaude** `01234567`\n\n>what does it do"
    );
    assert_eq!(sent[0].reply, json!(null), "the prompt opens the thread");
    // A prompt deleted from the chat leaves the answer to it a message of its own.
    let reply = replying_to(sent[0].id);
    assert!(
        sent[1..].iter().all(|call| call.reply == reply),
        "every message of the turn replies to the prompt"
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A prompt submitted while a turn is running is queued by Claude Code and reported
/// under the running turn's id, so the turn it eventually gets is announced by the
/// first event carrying an id of its own. Each turn must still answer its own prompt.
#[test]
fn a_prompt_queued_during_a_turn_gets_a_thread_of_its_own() {
    let (port, calls, _chat) = recorder();
    let root = prepare("queue", port);
    let resident = resident(&root);
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
        hook(&root, &event);
        if streaming {
            std::thread::sleep(Duration::from_millis(400));
        }
    }

    let made = collect(&calls, |call| {
        call.label == "sendRichMessage ring" && call.markdown.ends_with("\n\ntwo")
    });
    assert_eq!(
        holding(&made),
        [
            ("silent", ">first ask".to_owned()),
            ("silent", ">second ask".to_owned()),
            ("ring", "one".to_owned()),
            ("ring", "two".to_owned()),
        ]
    );
    let sent: Vec<&Call> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage "))
        .collect();
    // A queued prompt has no turn yet, so its head addresses the session alone.
    assert_eq!(sent[1].markdown, "**klaude** `fedcba98`\n\n>second ask");
    assert_eq!(
        sent[2].reply,
        replying_to(sent[0].id),
        "the first answer replies to the first ask"
    );
    assert_eq!(
        sent[3].reply,
        replying_to(sent[1].id),
        "the queued turn replies to the prompt that was queued"
    );
    assert!(
        sent[3]
            .markdown
            .starts_with("**klaude** `fedcba98/bbbbbbbb`"),
        "the queued turn's head reads {:?}",
        sent[3].markdown
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A turn that has said nothing is already on screen, so the minutes it spends thinking
/// or in tool calls read as the status line's own clock.
#[test]
fn a_turn_is_on_screen_before_it_has_said_anything() {
    let (port, calls, _chat) = recorder();
    let root = prepare("waiting", port);
    let resident = resident(&root);

    hook(
        &root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "0123456789abcdef",
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "prompt": "what does it do",
        }),
    );

    let made = collect(&calls, |call| call.markdown.contains('✻'));
    showing(&made.last().expect("a live message").markdown, "");
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A turn long enough to be watched puts a message up and rewrites it as it goes. The
/// segment that finishes there keeps that message, and the one the answer repeats is
/// taken back, so nothing the chat holds is said twice.
#[test]
fn a_segment_watched_while_it_ran_finishes_in_the_message_it_was_watched_in() {
    let (port, calls, _chat) = recorder();
    let root = prepare("watched", port);
    let resident = resident(&root);
    let session = "0123456789abcdef";

    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "think"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0, "delta": "half "}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 1, "delta": "a thought"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m2", "index": 0, "delta": "done"}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
    ];
    let last = turn.len() - 1;
    for (step, mut event) in turn.into_iter().enumerate() {
        event["session_id"] = json!(session);
        event["cwd"] = json!(env!("CARGO_MANIFEST_DIR"));
        hook(&root, &event);
        if step < last {
            // Longer than the gap the resident leaves between two rewrites, so every
            // step of the turn is one the chat was shown.
            std::thread::sleep(Duration::from_millis(3200));
        }
    }

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        holding(&made),
        [
            ("silent", ">think".to_owned()),
            ("silent", "half a thought".to_owned()),
            ("ring", "done".to_owned()),
        ]
    );
    let counted = |label: &str| made.iter().filter(|call| call.label == label).count();
    assert_eq!(
        counted("sendRichMessage silent"),
        // The prompt, then one message per segment, each of them watched before it was
        // finished rather than sent again once it was.
        3,
        "the chat took {:?}",
        made.iter().map(|call| &call.label).collect::<Vec<_>>()
    );
    assert_eq!(
        counted("deleteMessage"),
        1,
        "the last segment's message goes"
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A turn that talked, worked and talked again leaves three messages in order, and the
/// run of tool calls is the middle one.
#[test]
fn a_run_of_tool_calls_is_a_message_of_its_own() {
    let (port, calls, _chat) = recorder();
    let root = prepare("tools", port);
    let resident = resident(&root);
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
        hook(&root, &event);
        // What a turn says after a run of tool calls arrives once those calls have run,
        // which is far longer than the wait a call is filed after.
        std::thread::sleep(Duration::from_millis(400));
    }

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        holding(&made),
        [
            ("silent", ">run the tests".to_owned()),
            ("silent", "on it".to_owned()),
            (
                "silent",
                "× Bash `cargo test` 4s  \n⎿ Exit code 1  \n● [Explore] Read `/src/listen.rs` 12ms"
                    .to_owned()
            ),
            ("ring", "one test fails".to_owned()),
        ]
    );
    let sent: Vec<&Call> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage "))
        .collect();
    assert!(
        sent[2].markdown.starts_with("**klaude** `01234567/"),
        "the run's head reads {:?}",
        sent[2].markdown
    );
    assert_eq!(
        sent[2].reply,
        replying_to(sent[0].id),
        "the run threads under the prompt"
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// An assistant message's last flush reaches the resident after the hook of the tool
/// call that message ends with, so the words introducing a call are announced after it.
/// They belong above it in the chat all the same.
#[test]
fn a_call_announced_before_the_words_that_introduce_it_still_follows_them() {
    let (port, calls, _chat) = recorder();
    let root = prepare("settling", port);
    let resident = resident(&root);
    let session = "0123456789abcdef";

    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "go"}),
        json!({"hook_event_name": "PreToolUse", "tool_use_id": "t1", "tool_name": "Bash",
               "tool_input": {"command": "cargo test"}}),
        // The flush of the message that ends with that call, a few milliseconds behind.
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0,
               "final": true, "delta": "let me check"}),
        json!({"hook_event_name": "PostToolUse", "tool_use_id": "t1", "duration_ms": 30}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "checked"}),
    ];
    let last = turn.len() - 1;
    for (step, mut event) in turn.into_iter().enumerate() {
        event["session_id"] = json!(session);
        event["cwd"] = json!(env!("CARGO_MANIFEST_DIR"));
        hook(&root, &event);
        // The call, its words and its outcome arrive together; the turn ends later.
        if step == last - 1 {
            std::thread::sleep(Duration::from_millis(600));
        }
    }

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        holding(&made),
        [
            ("silent", ">go".to_owned()),
            ("silent", "let me check".to_owned()),
            ("silent", "● Bash `cargo test` 30ms".to_owned()),
            ("ring", "checked".to_owned()),
        ]
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
    let root = prepare("straggler", port);
    let resident = resident(&root);
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
        hook(&root, &event);
        std::thread::sleep(Duration::from_millis(400));
    }

    // Long enough for the message the turn the straggler would have opened puts up.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        holding(&drained(&calls)),
        [
            ("silent", ">say it".to_owned()),
            ("ring", "said it".to_owned()),
        ],
        "the answer is the last thing the turn says"
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A message that replies to nothing still names a session: the one heard from last.
/// These sessions run outside tmux, so what klaude says back names where the message
/// went and the terminal that session is on.
#[test]
fn a_message_replying_to_nothing_goes_to_the_session_heard_from_last() {
    let (port, calls, chat) = recorder();
    let root = prepare("unaddressed", port);
    let resident = resident(&root);

    // The later session sorts first, so what answers is the one heard from last
    // rather than the first one klaude happens to hold.
    for session in ["fedcba9876543210", "0123456789abcdef"] {
        hook(
            &root,
            &json!({
                "hook_event_name": "SessionStart",
                "session_id": session,
                "cwd": env!("CARGO_MANIFEST_DIR"),
            }),
        );
    }
    chat.says("carry on");

    let made = collect(&calls, |call| call.markdown.starts_with("`01234567` "));
    // The session reports the terminal the test itself was started from, and a build
    // machine may have given it none.
    let answer = made.last().expect("an answer").markdown.clone();
    let (address, why) = answer.split_once(' ').expect("an address and a reason");
    assert_eq!(address, "`01234567`");
    assert!(
        why == "has no terminal to type into"
            || (why.starts_with("is on `/dev/pts/")
                && why.ends_with("`, which no tmux pane holds")),
        "the answer reads {answer:?}"
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// What the chat is left holding: every message klaude sent, in the order it sent them,
/// carrying its last rewrite, without the ones it took back. Each is the sound it
/// arrived with and its body under the head.
fn holding(made: &[Call]) -> Vec<(&'static str, String)> {
    let mut order: Vec<i64> = Vec::new();
    let mut held: HashMap<i64, (&'static str, String)> = HashMap::new();
    let body = |call: &Call| {
        call.markdown
            .split_once("\n\n")
            .expect("a head and a body")
            .1
            .to_owned()
    };
    for call in made {
        match call.label.as_str() {
            "sendRichMessage silent" | "sendRichMessage ring" => {
                let sound = if call.label.ends_with("ring") {
                    "ring"
                } else {
                    "silent"
                };
                order.push(call.id);
                held.insert(call.id, (sound, body(call)));
            }
            "editMessageText" => {
                let target = call.target.expect("a message to rewrite");
                held.get_mut(&target).expect("a message klaude sent").1 = body(call);
            }
            "deleteMessage" => {
                let target = call.target.expect("a message to take back");
                order.retain(|held| *held != target);
            }
            _ => {}
        }
    }
    order
        .iter()
        .map(|id| held.remove(id).expect("a message"))
        .collect()
}

fn replying_to(message_id: i64) -> serde_json::Value {
    json!({"message_id": message_id, "allow_sending_without_reply": true})
}

/// One call the resident made, as the server saw it.
struct Call {
    label: String,
    markdown: String,
    reply: serde_json::Value,
    /// The message the call acts on, for a rewrite or a deletion.
    target: Option<i64>,
    /// What the server answered with, which is what a later message replies to.
    id: i64,
}

/// Every call the server has taken so far, in order, for a test that asserts what did
/// not happen and so cannot wait for a call to arrive.
fn drained(calls: &Receiver<Call>) -> Vec<Call> {
    calls.try_iter().collect()
}

/// Consecutive rewrites of one message collapse, because how many a segment takes is a
/// matter of how fast the deltas arrived.
fn collect(calls: &Receiver<Call>, done: impl Fn(&Call) -> bool) -> Vec<Call> {
    let deadline = Instant::now() + PATIENCE;
    let mut made: Vec<Call> = Vec::new();
    let mut finished = false;
    loop {
        let left = if finished {
            GRACE
        } else {
            deadline.saturating_duration_since(Instant::now())
        };
        let Ok(call) = calls.recv_timeout(left) else {
            assert!(
                finished,
                "the turn stopped after {:?}",
                made.iter().map(|call| &call.label).collect::<Vec<_>>()
            );
            return made;
        };
        finished |= done(&call);
        match made.last_mut() {
            Some(kept)
                if kept.label == "editMessageText"
                    && call.label == "editMessageText"
                    && kept.target == call.target =>
            {
                *kept = call;
            }
            _ => made.push(call),
        }
    }
}

/// The message showing an open segment opens with the head, carries what the segment
/// has said so far, and closes with the status line: a mark, a word and the turn's
/// elapsed time.
fn showing(markdown: &str, text: &str) {
    let (title, body) = markdown.split_once("\n\n").expect("a head and a body");
    assert!(title.starts_with("**klaude**"), "head reads {title:?}");
    let status = if text.is_empty() {
        body
    } else {
        body.strip_prefix(text)
            .and_then(|rest| rest.strip_prefix("  \n"))
            .expect("what was said and a status line")
    };
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

/// A throwaway root holding the runtime directory the resident binds its socket in and
/// the credentials file every klaude process started from it reads, so a machine's own
/// credentials stay out of the test.
fn prepare(name: &str, port: u16) -> PathBuf {
    let root = std::env::temp_dir().join(format!("klaude-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("run")).expect("runtime directory");
    std::fs::create_dir_all(root.join("config/klaude")).expect("configuration directory");
    std::fs::write(
        root.join("config/klaude/env"),
        format!("BOT_TOKEN=111111:secret\nCHAT_ID=1\nAPI_BASE=http://127.0.0.1:{port}\n"),
    )
    .expect("credentials");
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

fn resident(root: &Path) -> Resident {
    let child = klaude(root)
        .arg("listen")
        .spawn()
        .expect("run the resident");
    let socket = root.join("run/klaude/listen.sock");
    wait_for(|| socket.exists(), "the resident never bound its socket");
    Resident(child)
}

/// Both directories the binary resolves what it needs from point into the throwaway
/// root.
fn klaude(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_klaude"));
    command
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::piped());
    command
}

fn hook(root: &Path, event: &serde_json::Value) {
    let mut client = klaude(root).spawn().expect("run the hook");
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
        calls
            .send(Call {
                label: format!("{method}{sound}"),
                markdown: body["rich_message"]["markdown"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                reply: body["reply_parameters"].clone(),
                target: body["message_id"].as_i64(),
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

/// A message's last flushes race the hook of the tool call that ends it, so a delta can
/// land after klaude has already posted that message, carrying a paragraph rather than
/// a few characters. A tool reports late for the same reason, once the run holding it
/// is a message. Both belong in the message their segment became.
#[test]
fn a_flush_landing_after_its_message_was_posted_rewrites_that_message() {
    let (port, calls, _chat) = recorder();
    let root = prepare("straggling-flush", port);
    let resident = resident(&root);
    let session = "0123456789abcdef";

    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "go"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0, "delta": "on "}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 1, "delta": "it"}),
        json!({"hook_event_name": "PreToolUse", "tool_use_id": "t1", "tool_name": "Bash",
               "tool_input": {"command": "cargo test"}}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 2, "delta": " now"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m2", "index": 0, "delta": "done"}),
        json!({"hook_event_name": "PostToolUse", "tool_use_id": "t1", "duration_ms": 30}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
    ];
    for mut event in turn {
        event["session_id"] = json!(session);
        event["cwd"] = json!(env!("CARGO_MANIFEST_DIR"));
        hook(&root, &event);
        std::thread::sleep(Duration::from_millis(300));
    }

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        holding(&made),
        [
            ("silent", ">go"),
            // The flush that lost the race is written into the message it belongs to.
            ("silent", "on it now"),
            // So is the outcome of a call that reported after its run went out.
            ("silent", "● Bash `cargo test` 30ms"),
            ("ring", "done"),
        ]
        .map(|(sound, body)| (sound, body.to_owned()))
    );
    drop(resident);
    std::fs::remove_dir_all(&root).expect("clean up");
}

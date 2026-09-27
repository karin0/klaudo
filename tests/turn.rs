//! Drives the hook chain end to end against a server that answers like Telegram, so the
//! calls a turn makes, their order, what each replies to and the notification each
//! carries are checked without a network or a chat.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tempfile::TempDir;

const PATIENCE: Duration = Duration::from_secs(20);
/// How long a test waits for what follows the call it was watching for, so a rewrite or
/// a deletion issued right after the answer is part of what it reads.
const GRACE: Duration = Duration::from_millis(600);
/// The chat is a group, so the chat and the user klaude answers are two ids. The user's
/// private chat with the bot has the user's id. This repository is the project listed
/// for the group, so a session opened here posts there.
const GROUP: i64 = -1001;
const OWNER: i64 = 7;
const STRANGER: i64 = 8;

#[test]
fn a_turn_posts_the_prompt_and_replies_to_it_once_per_segment() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("segments", port);
    let root = temporary.path();
    let resident = resident(root);
    let session = "0123456789abcdef";

    hook(
        root,
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
            root,
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
        root,
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
}

/// A prompt submitted while a turn is running is queued by Claude Code and reported
/// under the running turn's id, so the turn it eventually gets is announced by the
/// first event carrying an id of its own. Each turn must still answer its own prompt.
#[test]
fn a_prompt_queued_during_a_turn_gets_a_thread_of_its_own() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("queue", port);
    let root = temporary.path();
    let resident = resident(root);
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
        hook(root, &event);
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
}

/// A turn that has said nothing is already on screen, so the minutes it spends thinking
/// or in tool calls read as the status line's own clock.
#[test]
fn a_turn_is_on_screen_before_it_has_said_anything() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("waiting", port);
    let root = temporary.path();
    let resident = resident(root);

    hook(
        root,
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
}

/// A turn long enough to be watched puts a message up and rewrites it as it goes. The
/// segment that finishes there keeps that message, and the one the answer repeats is
/// taken back, so nothing the chat holds is said twice.
#[test]
fn a_segment_watched_while_it_ran_finishes_in_the_message_it_was_watched_in() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("watched", port);
    let root = temporary.path();
    let resident = resident(root);
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
        hook(root, &event);
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
}

/// A final message arrives milliseconds before its `Stop`, so the message showing the
/// turn keeps its status line until the answer replaces it.
#[test]
fn an_answer_arriving_with_its_stop_is_shown_once() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("answered", port);
    let root = temporary.path();
    let resident = resident(root);
    let session = "0123456789abcdef";

    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "think"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0, "delta": "done"}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
    ];
    for (step, mut event) in turn.into_iter().enumerate() {
        event["session_id"] = json!(session);
        event["cwd"] = json!(env!("CARGO_MANIFEST_DIR"));
        hook(root, &event);
        if step == 0 {
            // Long enough for the message showing the turn to be up.
            std::thread::sleep(Duration::from_millis(3200));
        }
    }

    let made = collect(&calls, |call| call.label == "deleteMessage");
    let rewrites: Vec<_> = made
        .iter()
        .filter(|call| call.label == "editMessageText")
        .map(|call| &call.markdown)
        .collect();
    assert!(
        rewrites.is_empty(),
        "the live message was rewritten with {rewrites:?}"
    );
    assert_eq!(
        holding(&made),
        [("silent", ">think".to_owned()), ("ring", "done".to_owned())]
    );
    drop(resident);
}

/// A compaction Claude Code started mid-turn is a quiet note in that turn, and a
/// `/compact` rings as the answer to it, both quoting the summary and its reasoning.
#[test]
fn a_compaction_reports_its_summary() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("compacted", port);
    let root = temporary.path();
    let resident = resident(root);
    let summary = "<analysis>\nwhy\n</analysis>\n\n<summary>\nall of it\n</summary>";

    let events = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "work"}),
        json!({"hook_event_name": "PostCompact", "trigger": "auto", "compact_summary": summary}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
        json!({"hook_event_name": "PostCompact", "trigger": "manual", "compact_summary": summary}),
    ];
    for mut event in events {
        event["session_id"] = json!("0123456789abcdef");
        event["cwd"] = json!(env!("CARGO_MANIFEST_DIR"));
        hook(root, &event);
    }

    let made = collect(&calls, |call| {
        call.label == "sendRichMessage ring" && call.markdown.contains("#compact")
    });
    let quoted = "<blockquote expandable>all of it<cite>summary</cite></blockquote>\n\n\
                  <blockquote expandable>why<cite>analysis</cite></blockquote>"
        .to_owned();
    assert_eq!(
        holding(&made),
        [
            ("silent", ">work".to_owned()),
            ("silent", quoted.clone()),
            ("ring", "done".to_owned()),
            ("ring", quoted),
        ]
    );
    drop(resident);
}

/// A turn that talked, worked and talked again leaves three messages in order, and the
/// run of tool calls is the middle one.
#[test]
fn a_run_of_tool_calls_is_a_message_of_its_own() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("tools", port);
    let root = temporary.path();
    let resident = resident(root);
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
        hook(root, &event);
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
                "× **Bash**  `cargo test` **4s**  \n⎿ Exit code 1  \n● [Explore] **Read**  `/src/listen.rs` **12ms**"
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
}

/// An assistant message's last flush reaches the resident after the hook of the tool
/// call that message ends with, so the words introducing a call are announced after it.
/// They belong above it in the chat all the same.
#[test]
fn a_call_announced_before_the_words_that_introduce_it_still_follows_them() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("settling", port);
    let root = temporary.path();
    let resident = resident(root);
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
        hook(root, &event);
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
            ("silent", "● **Bash**  `cargo test` **30ms**".to_owned()),
            ("ring", "checked".to_owned()),
        ]
    );
    drop(resident);
}

/// The three hook processes run at once, so a delta can land after the `Stop` of its own
/// turn. Opening a second turn for it would leave a draft beside the answer showing
/// something else, and carrying the status line under it.
#[test]
fn a_delta_landing_after_its_stop_opens_no_second_turn() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("straggler", port);
    let root = temporary.path();
    let resident = resident(root);
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
        hook(root, &event);
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
}

/// A message that replies to nothing still names a session: the one heard from last in
/// the chat it was sent in. These sessions run outside tmux, so what klaude says back
/// names where the message went and the terminal that session is on. A reply to a
/// message that names no session goes nowhere.
#[test]
fn a_message_replying_to_nothing_goes_to_the_session_heard_from_last_in_its_chat() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("unaddressed", port);
    let root = temporary.path();
    let resident = resident(root);

    // The later session sorts first, so what answers is the one heard from last
    // rather than the first one klaude happens to hold. The throwaway root is outside
    // `CHAT_PROJECTS`, so the session there, heard from last of all, is the private
    // chat's.
    for (session, cwd) in [
        ("fedcba9876543210", Path::new(env!("CARGO_MANIFEST_DIR"))),
        ("0123456789abcdef", Path::new(env!("CARGO_MANIFEST_DIR"))),
        ("89abcdef01234567", root),
    ] {
        hook(
            root,
            &json!({
                "hook_event_name": "SessionStart",
                "session_id": session,
                "cwd": cwd,
            }),
        );
    }
    // Only the configured user is answered, whoever else shares the group, and the
    // answer goes back to the chat the message came from.
    chat.says(GROUP, STRANGER, "carry on");
    chat.says(GROUP, OWNER, "carry on");
    // A reply goes where the message it replies to says, and a message that names no
    // session, such as a later file of an album, is no reason to guess one.
    chat.replies(
        GROUP,
        OWNER,
        "carry on",
        &json!({"message_id": 5, "text": "a file"}),
    );
    chat.says(OWNER, OWNER, "carry on");

    let made = collect(&calls, |call| call.chat == Some(OWNER));
    let unnamed = "the message replied to names no session";
    assert_eq!(
        made.iter()
            .filter(|call| call.markdown == unnamed)
            .map(|call| call.chat)
            .collect::<Vec<_>>(),
        [Some(GROUP)]
    );
    let answers: Vec<_> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage") && call.markdown != unnamed)
        .map(|call| {
            let (address, why) = call
                .markdown
                .split_once(' ')
                .expect("an address and a reason");
            // The session reports the terminal the test itself was started from, and a
            // build machine may have given it none.
            assert!(
                why == "has no terminal to type into"
                    || (why.starts_with("is on `/dev/pts/")
                        && why.ends_with("`, which no tmux pane holds")),
                "the answer reads {:?}",
                call.markdown
            );
            (call.chat, address)
        })
        .collect();
    assert_eq!(
        answers,
        [(Some(GROUP), "`01234567`"), (Some(OWNER), "`89abcdef`")],
        "only the owner's messages are answered, each by a session of its own chat"
    );
    drop(resident);
}

/// A topic holds its own conversations: a message there reaches only a session whose
/// home is that topic, everything said back goes into the topic it answers, and a turn
/// the terminal starts follows the session to the topic it last posted in.
#[test]
fn a_topic_holds_its_own_conversations() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("topic", port);
    let root = temporary.path();
    let session = "0123456789abcdef";
    let seen = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis();
    // The session last posted in topic 77 of the group, and its process is the test.
    std::fs::create_dir_all(root.join("run/klaude")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaude/state.json"),
        json!({
            "sessions": [{
                "id": session,
                "dir": env!("CARGO_MANIFEST_DIR"),
                "pid": std::process::id(),
                "pane": null,
                "seen": seen,
                "trail": {"prompt": "", "last": [{"chat": GROUP, "topic": 77}, 5]},
            }],
            "ended": [],
        })
        .to_string(),
    )
    .expect("the state");
    let resident = resident(root);

    chat.says_in(GROUP, 78, OWNER, "anyone");
    chat.says(GROUP, OWNER, "anyone");
    chat.says_in(GROUP, 77, OWNER, "anyone");
    let made = collect(&calls, |call| call.markdown.starts_with("`01234567` "));
    let answered: Vec<_> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage"))
        .map(|call| {
            (
                call.markdown.split_once(' ').expect("a reason").0,
                call.body["message_thread_id"].as_i64(),
            )
        })
        .collect();
    assert_eq!(
        answered,
        [("no", Some(78)), ("no", None), ("`01234567`", Some(77))]
    );

    hook(
        root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "prompt": "from the terminal",
        }),
    );
    hook(
        root,
        &json!({
            "hook_event_name": "Stop",
            "session_id": session,
            "last_assistant_message": "done",
        }),
    );
    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    let sent: Vec<_> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage "))
        .collect();
    assert_eq!(sent.len(), 2, "the prompt and the answer");
    assert!(
        sent.iter()
            .all(|call| call.body["message_thread_id"] == json!(77)),
        "the turn stays in the topic"
    );
    assert_eq!(sent[1].reply, replying_to(sent[0].id));
    drop(resident);
}

/// A session killed mid-turn sends no event again, and the resident still finds it gone:
/// the message that showed the turn running is rewritten to what the turn said.
#[test]
fn a_turn_whose_session_was_killed_stops_reading_as_running() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("killed", port);
    let root = temporary.path();
    let resident = resident(root);

    // Each hook's parent is the session, and every one of these shells exits once its
    // hook has.
    for event in [
        json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "0123456789abcdef",
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "prompt": "what does it do",
        }),
        json!({
            "hook_event_name": "MessageDisplay",
            "session_id": "0123456789abcdef",
            "message_id": "m1",
            "index": 0,
            "delta": "said before dying",
        }),
    ] {
        let mut passing = within(root, "sh");
        passing.args(["-c", "\"$0\"; true", env!("CARGO_BIN_EXE_klaude")]);
        report(passing, &event);
    }

    let made = collect(&calls, |call| {
        call.label == "editMessageText" && !call.markdown.contains('✻')
    });
    let held = holding(&made);
    assert!(
        held.iter().any(|(_, body)| body == "said before dying"),
        "the chat holds {held:?}"
    );
    assert!(
        held.iter().all(|(_, body)| !body.contains('✻')),
        "the chat holds {held:?}"
    );
    drop(resident);
}

/// A session idle through a restart of the resident stays reachable, and so does one that
/// ended before it, though the resident that heard them was killed with no chance to
/// write anything on its way out.
#[test]
fn what_the_resident_knows_outlives_a_restart() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("restart", port);
    let root = temporary.path();
    let resident = resident(root);

    for (event, session, cwd) in [
        (
            "SessionStart",
            "0123456789abcdef",
            Path::new(env!("CARGO_MANIFEST_DIR")),
        ),
        ("SessionStart", "fedcba9876543210", root),
        ("SessionEnd", "fedcba9876543210", root),
    ] {
        hook(
            root,
            &json!({"hook_event_name": event, "session_id": session, "cwd": cwd}),
        );
    }
    // Answered once the events ahead of it are in, and the private chat has no
    // session left.
    chat.says(OWNER, OWNER, "anyone");
    collect(&calls, |call| {
        call.markdown.starts_with("no session is running here")
    });
    drop(resident);
    std::fs::remove_file(root.join("run/klaude/listen.sock")).expect("the old socket");
    let resident = self::resident(root);

    chat.replies(
        OWNER,
        OWNER,
        "pick it up",
        &json!({"rich_message": {"blocks": [
            {"type": "paragraph", "text": [{"type": "code", "text": "fedcba98"}]},
        ]}}),
    );
    chat.says(GROUP, OWNER, "carry on");
    collect(&calls, |call| {
        call.chat == Some(GROUP) && call.markdown.starts_with("`01234567` ")
    });
    let log = std::fs::read_to_string(root.join("tmux.log")).expect("tmux was called");
    assert!(
        log.lines()
            .any(|line| line.starts_with("new-session")
                && line.ends_with("--resume fedcba9876543210")),
        "tmux was called as {log:?}"
    );
    drop(resident);
}

/// A reply to a session that has exited, whether its process is gone or it reported its
/// end, opens a window resuming it in the directory it ran in, and a second reply before
/// that session starts waits for the same window.
#[test]
fn a_reply_to_a_session_that_exited_resumes_it() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("resume", port);
    let root = temporary.path();
    let resident = resident(root);

    // The hook's parent is the session, and this shell exits once the hook has.
    let mut passing = within(root, "sh");
    passing.args(["-c", "\"$0\"; true", env!("CARGO_BIN_EXE_klaude")]);
    report(
        passing,
        &json!({
            "hook_event_name": "SessionStart",
            "session_id": "0123456789abcdef",
            "cwd": root,
        }),
    );
    // The shell has exited, and a reply looks for the session only when it arrives.
    // This one's process is the test itself, which outlives it.
    for event in ["SessionStart", "SessionEnd"] {
        hook(
            root,
            &json!({
                "hook_event_name": event,
                "session_id": "fedcba9876543210",
                "cwd": root,
            }),
        );
    }
    let replied = json!({"rich_message": {"blocks": [
        {"type": "paragraph", "text": [
            {"type": "bold", "text": "klaude"}, " ", {"type": "code", "text": "01234567/89abcdef"},
        ]},
    ]}});
    chat.replies(OWNER, OWNER, "pick it up", &replied);
    chat.replies(OWNER, OWNER, "and then", &replied);
    chat.replies(
        OWNER,
        OWNER,
        "and that",
        &json!({"rich_message": {"blocks": [
            {"type": "paragraph", "text": [{"type": "code", "text": "fedcba98"}]},
        ]}}),
    );
    chat.replies(
        OWNER,
        OWNER,
        "and this",
        &json!({"caption": "klaude 77777777", "caption_entities": [
            {"type": "code", "offset": 7, "length": 8},
        ]}),
    );

    let made = collect(&calls, |call| call.chat == Some(OWNER));
    let said: Vec<_> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage"))
        .map(|call| call.markdown.as_str())
        .collect();
    assert_eq!(said, ["`77777777` is not a session this resident has seen"]);
    let log = std::fs::read_to_string(root.join("tmux.log")).expect("tmux was called");
    let windows: Vec<_> = log
        .lines()
        .filter(|line| line.starts_with("new-"))
        .collect();
    let window = |opening: &str, id: &str| {
        format!(
            "{opening} -c {} -n {} claude --resume {id}",
            root.display(),
            root.file_name().expect("a name").display()
        )
    };
    // The first window opens the session, which a later one joins.
    assert_eq!(
        windows,
        [
            window("new-session -d -s klaude", "0123456789abcdef"),
            window("new-window -t =klaude:", "fedcba9876543210"),
        ]
    );

    // The resumed session starts outside tmux, so each reply it takes is answered with
    // why it could not be typed.
    hook(
        root,
        &json!({
            "hook_event_name": "SessionStart",
            "session_id": "0123456789abcdef",
            "cwd": root,
        }),
    );
    let made = collect(&calls, |call| call.markdown.starts_with("`01234567` "));
    let taken = made
        .iter()
        .filter(|call| call.markdown.starts_with("`01234567` "))
        .count();
    assert_eq!(taken, 2, "both replies wait for the resumed session");
    drop(resident);
}

/// `/new` alone offers the projects that ran in its chat, the one heard from last first and
/// an exited one among them, and a press on one rewrites the menu into the anchor for it.
#[test]
fn a_new_conversation_opens_in_a_project_picked_from_a_menu() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("menu", port);
    let root = temporary.path();
    let resident = resident(root);

    for dir in ["a", "b"] {
        std::fs::create_dir(root.join(dir)).expect("a project");
    }
    for (event, session, cwd) in [
        ("SessionStart", "aaaaaaaa", root.join("a")),
        ("SessionStart", "bbbbbbbb", root.join("b")),
        (
            "SessionStart",
            "cccccccc",
            env!("CARGO_MANIFEST_DIR").into(),
        ),
        ("SessionEnd", "aaaaaaaa", root.join("a")),
    ] {
        hook(
            root,
            &json!({"hook_event_name": event, "session_id": session, "cwd": cwd}),
        );
    }
    chat.says(OWNER, OWNER, "/new");
    let made = collect(&calls, |call| call.label == "sendMessage");
    let menu = made.last().expect("the menu");
    let labels: Vec<_> = menu.body["reply_markup"]["inline_keyboard"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row[0]["text"].as_str().expect("a label").to_owned())
        .collect();
    let a = root.join("a").display().to_string();
    let b = root.join("b").display().to_string();
    assert_eq!(labels, [a.clone(), b.clone()]);
    assert_eq!(menu.chat, Some(OWNER));

    chat.presses(OWNER, menu, "new 1");
    let made = collect(&calls, |call| call.label == "deleteMessage");
    let anchor = made
        .iter()
        .find(|call| call.label.starts_with("sendRichMessage"))
        .expect("the anchor");
    // The directory is escaped as prose, which the reader never sees.
    assert_eq!(
        anchor.markdown.replace('\\', ""),
        format!("**b** `new`\n\n{b}")
    );
    // The next message typed replies to the anchor.
    assert_eq!(
        anchor.body["reply_markup"],
        json!({"force_reply": true, "input_field_placeholder": format!("first prompt in {b}")
            .chars().take(64).collect::<String>()})
    );
    assert_eq!(
        made.last().expect("the menu taken back").target,
        Some(menu.id)
    );
    assert!(made.iter().any(|call| call.label == "answerCallbackQuery"));

    // The group's menu holds only the project posting there.
    chat.says(GROUP, OWNER, "/new@klaude_bot");
    let made = collect(&calls, |call| call.label == "sendMessage");
    let menu = made.last().expect("the menu");
    assert_eq!(
        menu.body["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
        "new 0"
    );
    assert_eq!(
        menu.body["reply_markup"]["inline_keyboard"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    drop(resident);
}

/// `/resume` offers the projects of its chat, then the sessions of the one picked, the one
/// heard from last first, and a press on a session posts an anchor addressed to it that
/// replies to the last message it left, taking the menu back.
#[test]
fn a_conversation_is_resumed_from_a_menu_of_its_project() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("resume-menu", port);
    let root = temporary.path();
    let resident = resident(root);

    let first = "1111111111111111";
    for event in [
        json!({"hook_event_name": "UserPromptSubmit", "session_id": first, "cwd": root,
            "prompt": "tidy up the build scripts and nothing else\nsecond line"}),
        json!({"hook_event_name": "Stop", "session_id": first, "cwd": root,
            "last_assistant_message": "tidied"}),
        json!({"hook_event_name": "SessionEnd", "session_id": first, "cwd": root}),
        json!({"hook_event_name": "SessionStart", "session_id": "2222222222222222", "cwd": root}),
    ] {
        hook(root, &event);
    }
    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    let answer = made.last().expect("the answer").id;

    chat.says(OWNER, OWNER, "/resume");
    let made = collect(&calls, |call| call.label == "sendMessage");
    chat.presses(OWNER, made.last().expect("the menu"), "resume 0");
    let made = collect(&calls, |call| call.label == "editMessageText");
    let menu = made.last().expect("the sessions");
    assert_eq!(
        menu.markdown,
        format!("Resume a conversation in {}:", root.display())
    );
    let buttons: Vec<_> = menu.body["reply_markup"]["inline_keyboard"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| {
            (
                row[0]["text"].as_str().expect("a label"),
                row[0]["callback_data"].as_str().expect("its data"),
            )
        })
        .collect();
    let [(latest, _), (earlier, data)] = buttons[..] else {
        panic!("the menu holds {buttons:?}");
    };
    assert!(latest.starts_with("22222222 · "), "{latest:?}");
    assert!(
        earlier.starts_with("11111111 · ")
            && earlier.ends_with(" ago · tidy up the build scripts and nothing el…"),
        "{earlier:?}"
    );
    assert_eq!(data, format!("session {first}"));

    chat.presses(OWNER, menu, data);
    let made = collect(&calls, |call| call.label == "deleteMessage");
    let anchor = made
        .iter()
        .find(|call| call.label.starts_with("sendRichMessage"))
        .expect("the anchor");
    assert!(anchor.markdown.starts_with("**"), "{:?}", anchor.markdown);
    assert!(
        anchor.markdown.contains(" `11111111`\n\n"),
        "{:?}",
        anchor.markdown
    );
    assert_eq!(anchor.reply, replying_to(answer));
    assert_eq!(
        anchor.body["reply_markup"],
        json!({"force_reply": true, "input_field_placeholder": "prompt for 11111111"})
    );
    assert_eq!(
        made.last().expect("the menu taken back").target,
        menu.target
    );
    drop(resident);
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
    /// A message's markdown, the caption of a file, or the text of a menu.
    markdown: String,
    /// What an uploaded file holds.
    document: Option<String>,
    reply: serde_json::Value,
    /// The message the call acts on, for a rewrite or a deletion.
    target: Option<i64>,
    chat: Option<i64>,
    /// The whole request, for what the fields above leave out.
    body: serde_json::Value,
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
/// credentials stay out of the test. It is removed when the test drops it, which a
/// failing test does too.
fn prepare(name: &str, port: u16) -> TempDir {
    let temporary = tempfile::Builder::new()
        .prefix(&format!("klaude-{name}-"))
        .tempdir()
        .expect("throwaway root");
    let root = temporary.path();
    std::fs::create_dir_all(root.join("run")).expect("runtime directory");
    // A window klaude opens goes to a `tmux` that records how it was called, so a test
    // never reaches the tmux server of the machine it runs on. Its session exists once a
    // `new-session` has been recorded.
    std::fs::create_dir_all(root.join("bin")).expect("binary directory");
    let tmux = root.join("bin/tmux");
    std::fs::write(
        &tmux,
        "#!/bin/sh\nlog=\"$(dirname \"$0\")/../tmux.log\"\nif [ \"$1\" = has-session ]; then grep -q '^new-session' \"$log\" 2>/dev/null; exit; fi\nprintf '%s\\n' \"$*\" >> \"$log\"\n",
    )
    .expect("a recording tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755))
        .expect("an executable tmux");
    std::fs::create_dir_all(root.join("config/klaude")).expect("configuration directory");
    std::fs::write(
        root.join("config/klaude/env"),
        format!("BOT_TOKEN=111111:secret\nCHAT_ID={GROUP}\nUSER_ID={OWNER}\nCHAT_PROJECTS={}\nAPI_BASE=http://127.0.0.1:{port}\n", env!("CARGO_MANIFEST_DIR")),
    )
    .expect("credentials");
    temporary
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

fn klaude(root: &Path) -> Command {
    within(root, env!("CARGO_BIN_EXE_klaude"))
}

/// Both directories the binary resolves what it needs from point into the throwaway
/// root, and so does the first `tmux` on the path.
fn within(root: &Path, program: &str) -> Command {
    let path = std::env::join_paths(std::iter::once(root.join("bin")).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("a path");
    let mut command = Command::new(program);
    command
        .env("PATH", path)
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .stdin(Stdio::piped());
    command
}

fn hook(root: &Path, event: &serde_json::Value) {
    report(klaude(root), event);
}

/// The last answer with how long ago each figure was reported left out, which is as long
/// as the test took getting there.
fn ageless(made: &[Call]) -> String {
    let answer = &made.last().expect("the answer").markdown;
    let (said, reported) = answer.rsplit_once('\n').expect("a line of ages");
    let ages: Vec<String> = reported
        .strip_prefix("reported: ")
        .expect("the ages")
        .split(", ")
        .map(|age| {
            let (figure, ago) = age.split_once(' ').expect("a figure and its age");
            let amount = ago.strip_suffix(" ago").expect("an age");
            assert!(
                amount.len() > 1
                    && amount[..amount.len() - 1]
                        .bytes()
                        .all(|b| b.is_ascii_digit())
            );
            format!("{figure} <ago>")
        })
        .collect();
    format!("{said}\nreported: {}", ages.join(", "))
}

/// What Claude Code hands a session's status line.
fn status(root: &Path, input: &serde_json::Value) {
    let mut command = klaude(root);
    command.arg("status");
    report(command, input);
}

fn report(mut command: Command, event: &serde_json::Value) {
    let mut client = command.spawn().expect("run the hook");
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
    /// A message from the phone, as Telegram delivers it. It replies to nothing, which
    /// is what makes it a message to route by itself.
    fn says(&self, chat: i64, sender: i64, text: &str) {
        self.replies(chat, sender, text, &serde_json::Value::Null);
    }

    /// A reply to `replied`, a message klaude posted as Telegram hands it back.
    fn replies(&self, chat: i64, sender: i64, text: &str, replied: &serde_json::Value) {
        self.push(json!({
            "chat": {"id": chat},
            "from": {"id": sender},
            "text": text,
            "reply_to_message": replied,
        }));
    }

    /// A message in topic `topic` replying to nothing, which a forum hands over as a
    /// reply to the service message that opened the topic.
    fn says_in(&self, chat: i64, topic: i64, sender: i64, text: &str) {
        self.push(json!({
            "chat": {"id": chat},
            "from": {"id": sender},
            "text": text,
            "message_thread_id": topic,
            "is_topic_message": true,
            "reply_to_message": {
                "message_id": topic,
                "message_thread_id": topic,
                "forum_topic_created": {"name": "a topic", "icon_color": 7_322_096},
            },
        }));
    }

    fn push(&self, mut message: serde_json::Value) {
        message["message_id"] = json!(9000);
        message["date"] = json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("a clock after 1970")
                .as_secs()
        );
        self.0
            .lock()
            .expect("the chat")
            .push(json!({"update_id": 1, "message": message}));
    }

    /// A press on a button of `menu`, a menu klaude posted or rewrote, as Telegram hands
    /// it back.
    fn presses(&self, sender: i64, menu: &Call, data: &str) {
        self.0.lock().expect("the chat").push(json!({
            "update_id": 1,
            "callback_query": {
                "id": "query",
                "from": {"id": sender},
                "message": {
                    "message_id": menu.target.unwrap_or(menu.id),
                    "chat": {"id": menu.chat},
                    "text": menu.markdown,
                    "reply_markup": menu.body["reply_markup"],
                },
                "data": data,
            },
        }));
    }

    fn drain(&self) -> Vec<serde_json::Value> {
        std::mem::take(&mut *self.0.lock().expect("the chat"))
    }
}

fn answer(mut stream: TcpStream, id: i64, calls: &Sender<Call>, chat: &Chat) {
    let mut received = Vec::new();
    let mut chunk = [0; 4096];
    let (method, length, boundary, head) = loop {
        let read = stream.read(&mut chunk).expect("request");
        // A resident killed on its way to the next request leaves nothing to answer.
        if read == 0 {
            return;
        }
        received.extend_from_slice(&chunk[..read]);
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut request = httparse::Request::new(&mut headers);
        let httparse::Status::Complete(head) = request.parse(&received).expect("an HTTP request")
        else {
            continue;
        };
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case(name))
                .map(|header| String::from_utf8_lossy(header.value).into_owned())
        };
        let method = request
            .path
            .and_then(|path| path.rsplit('/').next())
            .expect("method")
            .to_owned();
        let length =
            header("content-length").map_or(0, |value| value.parse().expect("content length"));
        let boundary = header("content-type")
            .and_then(|value| Some(value.split_once("boundary=")?.1.to_owned()));
        break (method, length, boundary, head);
    };
    let mut body = received.split_off(head);
    let arrived = body.len();
    body.resize(length, 0);
    stream.read_exact(&mut body[arrived..]).expect("body");
    let body = match boundary {
        Some(boundary) => form(&String::from_utf8(body).expect("a text form"), &boundary),
        None => serde_json::from_slice(&body).expect("a JSON body"),
    };

    // The poll that asks what the chat said is not a call the turn made.
    let sent = if method == "getUpdates" {
        json!({"ok": true, "result": chat.drain()}).to_string()
    } else {
        let sound = match method.as_str() {
            "sendRichMessage" | "sendDocument" | "sendMediaGroup"
                if body["disable_notification"] == json!(true) =>
            {
                " silent"
            }
            "sendRichMessage" | "sendDocument" | "sendMediaGroup" => " ring",
            _ => "",
        };
        calls
            .send(Call {
                label: format!("{method}{sound}"),
                markdown: body["rich_message"]["markdown"]
                    .as_str()
                    .or(body["caption"].as_str())
                    .or(body["text"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
                document: body["document"].as_str().map(str::to_owned),
                reply: body["reply_parameters"].clone(),
                target: body["message_id"].as_i64(),
                chat: body["chat_id"].as_i64(),
                body: body.clone(),
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

/// A multipart form as the JSON body the same call would carry otherwise: a field that
/// reads as JSON is that value, and anything else is its text.
fn form(body: &str, boundary: &str) -> serde_json::Value {
    let mut fields = serde_json::Map::new();
    for part in body.split(&format!("--{boundary}")) {
        let Some((headers, value)) = part.split_once("\r\n\r\n") else {
            continue;
        };
        let name = headers
            .split("name=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("a field name");
        let value = value.strip_suffix("\r\n").unwrap_or(value);
        let value = serde_json::from_str(value).unwrap_or_else(|_| json!(value));
        fields.insert(name.to_owned(), value);
    }
    fields.into()
}

/// A call the binary cannot act on says how to call it, the help of the binary ends with
/// how `klaude send` is called, and a file that is not there is named, both without a
/// panic.
#[test]
fn the_command_line_explains_itself() {
    let temporary = prepare("usage", 0);
    let root = temporary.path();
    let run = |args: &[&str]| klaude(root).args(args).output().expect("run klaude");

    for (asked, usage) in [
        (&["--help"][..], "Usage: klaude [COMMAND]"),
        (&["-h"], "Usage: klaude [COMMAND]"),
        (&["send", "--help"], "Usage: klaude send <FILES>..."),
    ] {
        let help = run(asked);
        assert!(help.status.success(), "{asked:?}");
        assert!(
            String::from_utf8_lossy(&help.stdout).contains(usage),
            "{asked:?}"
        );
    }
    let help = run(&["--help"]);
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("\n\nUsage: klaude send <FILES>...\n\n"),
        "{help:?}"
    );

    for wrong in [&["send"][..], &["sned", "a"]] {
        let misused = run(wrong);
        assert_eq!(misused.status.code(), Some(2), "{wrong:?}");
        assert!(
            String::from_utf8_lossy(&misused.stderr).starts_with("error: "),
            "{wrong:?}"
        );
    }

    let file = root.join("build.log");
    std::fs::write(&file, "all green").expect("the file to send");
    let path = file.to_str().expect("a UTF-8 path");
    let missing = run(&["send", path, "/definitely/not/here"]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&missing.stderr),
        "klaude: /definitely/not/here: No such file or directory (os error 2)\n"
    );

    // Nothing is listening in this root, which is what a stopped resident looks like.
    let alone = run(&["send", path]);
    assert_eq!(alone.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&alone.stderr).starts_with("klaude: the resident at "));
}

/// The commands klaude answers are listed in the command menu of each chat, for the user
/// alone.
#[test]
fn the_commands_are_listed_for_the_user_in_both_chats() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("commands", port);
    let resident = resident(temporary.path());

    let made = collect(&calls, |call| call.body["scope"]["type"] == "chat_member");
    let registered: Vec<_> = made
        .iter()
        .filter(|call| call.label == "setMyCommands")
        .collect();
    assert!(
        registered
            .iter()
            .all(|call| call.body["commands"][0]["command"] == "new")
    );
    let scopes: Vec<_> = registered.iter().map(|call| &call.body["scope"]).collect();
    assert_eq!(
        scopes,
        [
            &json!({"type": "chat", "chat_id": OWNER}),
            &json!({"type": "chat_member", "chat_id": GROUP, "user_id": OWNER}),
        ]
    );
    drop(resident);
}

/// `/usage` answers from what the status lines last reported: the plan's limits from any
/// session, and the context of the session a message would reach, under that session's
/// head.
#[test]
fn usage_is_answered_from_the_status_lines() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("status", port);
    let root = temporary.path();
    let resident = resident(root);

    // No session is here to go with the limits.
    chat.says(OWNER, OWNER, "/usage");
    let made = collect(&calls, |call| call.label == "sendMessage");
    let unreported = made.last().expect("the answer");
    assert_eq!(unreported.markdown, "limits: not reported yet");
    assert_eq!(unreported.reply["message_id"], 9000);

    std::fs::create_dir(root.join("a")).expect("a project");
    for session in ["aaaaaaaa", "bbbbbbbb"] {
        hook(
            root,
            &json!({"hook_event_name": "SessionStart", "session_id": session, "cwd": root.join("a")}),
        );
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    status(
        root,
        &json!({
            "session_id": "aaaaaaaa",
            "context_window": {
                "context_window_size": 1_000_000,
                "current_usage": {
                    "input_tokens": 2,
                    "output_tokens": 126,
                    "cache_creation_input_tokens": 19957,
                    "cache_read_input_tokens": 25597,
                },
                "used_percentage": 5,
            },
            "rate_limits": {
                "five_hour": {"used_percentage": 1, "resets_at": now + 3 * 3600 + 29 * 60 + 30},
                "seven_day": {"used_percentage": 56.4, "resets_at": now + 62 * 3600 + 30},
            },
        }),
    );
    status(
        root,
        &json!({
            "session_id": "bbbbbbbb",
            "context_window": {
                "context_window_size": 200_000,
                "current_usage": null,
                "used_percentage": null,
            },
        }),
    );

    // Unaddressed, it reaches the session heard from last.
    chat.says(OWNER, OWNER, "/usage");
    let made = collect(&calls, |call| call.label == "sendMessage");
    let bar = |bar| format!("<code>{bar}</code>  ");
    let limits = limits(now);
    assert_eq!(
        ageless(&made),
        format!("<b>a</b> <code>bbbbbbbb</code>\ncontext: nothing has been sent yet\n{limits}")
    );
    chat.replies(
        OWNER,
        OWNER,
        "/usage@klaude_bot",
        &json!({
            "text": "a bbbbbbbb\n\ncontext: nothing has been sent yet",
            "entities": [{"type": "bold", "offset": 0, "length": 1}, {"type": "code", "offset": 2, "length": 8}],
        }),
    );
    let made = collect(&calls, |call| call.label == "sendMessage");
    assert!(
        ageless(&made).starts_with("<b>a</b> <code>bbbbbbbb</code>"),
        "an answer is addressed like any message"
    );
    chat.replies(
        OWNER,
        OWNER,
        "/usage",
        &json!({"rich_message": {"blocks": [
            {"type": "paragraph", "text": [{"type": "code", "text": "aaaaaaaa"}]},
        ]}}),
    );
    let made = collect(&calls, |call| call.label == "sendMessage");
    assert_eq!(
        ageless(&made),
        format!(
            "<b>a</b> <code>aaaaaaaa</code>\n{}context 5%, 45.6k of 1m\n{limits}",
            bar("▌░░░░░░░░░")
        )
    );
    drop(resident);
}

/// The limits `usage_is_answered_from_the_status_lines` reports at `now`, as the answer
/// writes them.
fn limits(now: u64) -> String {
    let bar = |bar| format!("<code>{bar}</code>  ");
    let resets = |unix: u64, left| {
        format!(
            "resets in {left}\n{}\
             <tg-time unix=\"{unix}\" format=\"wDt\">{} UTC</tg-time>",
            "\u{2002}".repeat(12) + "  ",
            clock(unix)
        )
    };
    let (five, seven) = (now + 3 * 3600 + 29 * 60 + 30, now + 62 * 3600 + 30);
    format!(
        "{}5-hour 1%, {}\n{}7-day 56%, {}\n\
         reported: context <ago>, limits <ago>",
        bar("▏░░░░░░░░░"),
        resets(five, "3h 30m"),
        bar("█████▋░░░░"),
        resets(seven, "2d 14h"),
    )
}

/// The time of day of a Unix time, in UTC.
fn clock(unix: u64) -> String {
    format!("{:02}:{:02}", unix % 86400 / 3600, unix % 3600 / 60)
}

/// Files a session sends land in the thread of the turn that sent them, below what the
/// turn has said, as an album in the order they were named, and `klaude send` exits with
/// how the uploads went.
#[test]
fn a_file_a_turn_sends_lands_in_its_thread() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("file", port);
    let root = temporary.path();
    let resident = resident(root);
    let session = "0123456789abcdef";

    hook(
        root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "prompt_id": "aaaaaaaa-1111",
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "prompt": "show me the log",
        }),
    );
    let file = root.join("build.log");
    std::fs::write(&file, "all green").expect("the file to send");
    let other = root.join("test.log");
    std::fs::write(&other, "all passed").expect("the file to send");
    let sent = klaude(root)
        .args(["send".as_ref(), file.as_os_str(), other.as_os_str()])
        .env("CLAUDE_CODE_SESSION_ID", session)
        .output()
        .expect("run send");
    assert!(sent.status.success(), "send failed: {sent:?}");

    let made = collect(&calls, |call| call.label.starts_with("sendMediaGroup"));
    let album = made.last().expect("the album");
    assert_eq!(album.label, "sendMediaGroup silent");
    assert_eq!(album.chat, Some(GROUP));
    assert_eq!(
        (&album.body["file0"], &album.body["file1"]),
        (&json!("all green"), &json!("all passed"))
    );
    let prompt = made
        .iter()
        .find(|call| call.label.starts_with("sendRichMessage"))
        .expect("the prompt");
    assert_eq!(album.reply, replying_to(prompt.id));
    let media = album.body["media"]
        .as_array()
        .expect("the album's documents");
    assert_eq!(
        media.iter().map(|item| &item["media"]).collect::<Vec<_>>(),
        [&json!("attach://file0"), &json!("attach://file1")]
    );
    let caption = media[0]["caption"].as_str().expect("a caption");
    assert!(
        caption.starts_with("<b>klaude</b> <code>01234567/aaaaaaaa</code> "),
        "the caption reads {caption:?}"
    );
    assert_eq!(media[0]["parse_mode"], json!("HTML"));
    assert_eq!(media[1].get("caption"), None);

    // A session klaude has never heard from has no thread to post in.
    let refused = klaude(root)
        .args(["send".as_ref(), file.as_os_str()])
        .env("CLAUDE_CODE_SESSION_ID", "ffffffff")
        .output()
        .expect("run send");
    assert!(!refused.status.success(), "send to nowhere succeeded");
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        "klaude: session ffffffff has not reported to klaude\n"
    );

    // Run outside Claude Code, it names no session, and the directory it runs in is no
    // project listed for the group, so the file goes to the user's private chat.
    let bare = klaude(root)
        .args(["send".as_ref(), file.as_os_str()])
        .current_dir(root)
        .output()
        .expect("run send");
    assert!(bare.status.success(), "send failed: {bare:?}");
    let made = collect(&calls, |call| call.label.starts_with("sendDocument"));
    let document = made.last().expect("the file");
    assert_eq!(document.document.as_deref(), Some("all green"));
    assert_eq!(document.chat, Some(OWNER));
    assert_eq!(document.reply, json!(null));
    assert_eq!(document.markdown, "");
    drop(resident);
}

/// A message's last flushes race the hook of the tool call that ends it, so a delta can
/// land after klaude has already posted that message, carrying a paragraph rather than
/// a few characters. A tool reports late for the same reason, once the run holding it
/// is a message. Both belong in the message their segment became.
#[test]
fn a_flush_landing_after_its_message_was_posted_rewrites_that_message() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("straggling-flush", port);
    let root = temporary.path();
    let resident = resident(root);
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
        hook(root, &event);
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
            ("silent", "● **Bash**  `cargo test` **30ms**"),
            ("ring", "done"),
        ]
        .map(|(sound, body)| (sound, body.to_owned()))
    );
    drop(resident);
}

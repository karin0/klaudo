//! Drives the hook chain end to end against a server that answers like Telegram, so the
//! calls a turn makes, their order, what each replies to and the notification each
//! carries are checked without a network or a chat.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tempfile::TempDir;

const PATIENCE: Duration = Duration::from_secs(20);
/// How long a test waits for what follows the call it was watching for, so a rewrite or
/// a deletion issued right after the answer is part of what it reads.
const GRACE: Duration = Duration::from_millis(600);
/// How long the server holds a poll open with nothing to report. A poll answered at
/// once has the daemon ask again at once, and the connections it closes pile up in
/// `TIME_WAIT` until the machine runs out of local ports.
const HOLD: Duration = Duration::from_secs(1);
/// The chat is a group, so the chat and the user Klaŭdo answers are two ids. The user's
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
    let daemon = daemon(root);
    let session = "0123456789abcdef";

    hook(
        root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "cwd": project(root),
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
        "**project** `01234567`\n\n>what does it do"
    );
    assert_eq!(sent[0].reply, json!(null), "the prompt opens the thread");
    // A prompt deleted from the chat leaves the answer to it a message of its own.
    let reply = replying_to(sent[0].id);
    assert!(
        sent[1..].iter().all(|call| call.reply == reply),
        "every message of the turn replies to the prompt"
    );
    drop(daemon);
}

/// A prompt submitted while a turn is running is queued by Claude Code and reported
/// under the running turn's id, so the turn it eventually gets is announced by the
/// first event carrying an id of its own. Each turn must still answer its own prompt.
#[test]
fn a_prompt_queued_during_a_turn_gets_a_thread_of_its_own() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("queue", port);
    let root = temporary.path();
    let daemon = daemon(root);
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
        event["cwd"] = json!(project(root));
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
    assert_eq!(sent[1].markdown, "**project** `fedcba98`\n\n>second ask");
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
            .starts_with("**project** `fedcba98/bbbbbbbb`"),
        "the queued turn's head reads {:?}",
        sent[3].markdown
    );
    drop(daemon);
}

/// A turn that has said nothing is already on screen, so the minutes it spends thinking
/// or in tool calls read as the status line's own clock.
#[test]
fn a_turn_is_on_screen_before_it_has_said_anything() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("waiting", port);
    let root = temporary.path();
    let daemon = daemon(root);

    hook(
        root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "0123456789abcdef",
            "cwd": project(root),
            "prompt": "what does it do",
        }),
    );

    let made = collect(&calls, |call| call.markdown.contains('✻'));
    showing(&made.last().expect("a live message").markdown, "");
    drop(daemon);
}

/// A running turn shows its session typing, again before the five seconds Telegram shows
/// each action for run out and as soon as a message it posts has cleared the last one,
/// except while a dialog waits on the user, and stops with the turn.
#[test]
fn a_running_turn_shows_its_session_typing() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("typing", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let send = |mut event: serde_json::Value| {
        event["session_id"] = json!("0123456789abcdef");
        event["cwd"] = json!(project(root));
        hook(root, &event);
    };
    let shown = |count: usize| {
        wait_for(|| chat.actions().len() >= count, "no chat action arrived");
    };
    // Past the gap between two actions, with a margin for a busy machine.
    let lapse = Duration::from_secs(5);

    send(json!({"hook_event_name": "UserPromptSubmit", "prompt": "what does it do"}));
    shown(2);
    let actions = chat.actions();
    assert_eq!(
        actions[0].1,
        json!({"chat_id": GROUP, "action": "typing"}),
        "the first action"
    );
    let gap = actions[1].0 - actions[0].0;
    assert!(gap < lapse, "two actions came {gap:?} apart");
    let made = collect(&calls, |call| call.markdown.contains('✻'));
    let live = made.last().expect("a live message").arrived;
    assert!(
        chat.actions()
            .iter()
            .any(|(at, _)| *at >= live && *at - live < Duration::from_millis(500)),
        "no action followed the live message"
    );

    send(json!({"hook_event_name": "Notification", "message": "Claude needs your permission"}));
    std::thread::sleep(GRACE);
    let paused = chat.actions().len();
    std::thread::sleep(lapse);
    assert_eq!(
        chat.actions().len(),
        paused,
        "typing went on behind a dialog"
    );

    send(
        json!({"hook_event_name": "PreToolUse", "tool_use_id": "t1", "tool_name": "Bash",
        "tool_input": {"command": "true"}}),
    );
    shown(paused + 1);

    send(json!({"hook_event_name": "Stop", "last_assistant_message": "done"}));
    std::thread::sleep(GRACE);
    let stopped = chat.actions().len();
    std::thread::sleep(lapse);
    assert_eq!(
        chat.actions().len(),
        stopped,
        "typing went on after the turn"
    );
    drop(daemon);
}

/// The figures that close an answer are under the status line while the turn runs, as
/// the session last reported them.
#[test]
fn a_running_turn_shows_the_status_line_figures() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("figures", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let session = "0123456789abcdef";

    hook(
        root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "cwd": project(root),
            "prompt": "what does it do",
        }),
    );

    status(
        root,
        &json!({
            "session_id": session,
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
        }),
    );
    let made = collect(&calls, |call| call.markdown.ends_with('`'));
    let live = &made.last().expect("a live message").markdown;
    let shown = live
        .strip_suffix("\n\n`5% 45.6k/1m`")
        .expect("the figures under the status line");
    showing(shown, "");
    drop(daemon);
}

/// A turn long enough to be watched puts a message up and rewrites it as it goes. The
/// segment that finishes there keeps that message, and the one the answer repeats is
/// taken back, so nothing the chat holds is said twice.
#[test]
fn a_segment_watched_while_it_ran_finishes_in_the_message_it_was_watched_in() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("watched", port);
    let root = temporary.path();
    let daemon = daemon(root);
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
        event["cwd"] = json!(project(root));
        hook(root, &event);
        if step < last {
            // Longer than the gap the daemon leaves between two rewrites, so every
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
    drop(daemon);
}

/// A final message arrives milliseconds before its `Stop`, so the message showing the
/// turn keeps its status line until the answer replaces it.
#[test]
fn an_answer_arriving_with_its_stop_is_shown_once() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("answered", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let session = "0123456789abcdef";

    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "think"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0, "delta": "done"}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
    ];
    for (step, mut event) in turn.into_iter().enumerate() {
        event["session_id"] = json!(session);
        event["cwd"] = json!(project(root));
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
    drop(daemon);
}

/// A compaction Claude Code started mid-turn is a quiet note in that turn, and a
/// `/compact` rings as the answer to it, both quoting the summary and its reasoning.
#[test]
fn a_compaction_reports_its_summary() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("compacted", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let summary = "<analysis>\nwhy\n</analysis>\n\n<summary>\nall of it\n</summary>";

    let events = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "work"}),
        json!({"hook_event_name": "PostCompact", "trigger": "auto", "compact_summary": summary}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
        json!({"hook_event_name": "PostCompact", "trigger": "manual", "compact_summary": summary}),
    ];
    for mut event in events {
        event["session_id"] = json!("0123456789abcdef");
        event["cwd"] = json!(project(root));
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
    drop(daemon);
}

/// A run of tool calls stands under the words that introduce it, so a turn that talked,
/// worked, talked and worked again leaves two messages ahead of its answer.
#[test]
fn a_run_of_tool_calls_stands_under_the_words_that_introduce_it() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("tools", port);
    let root = temporary.path();
    let daemon = daemon(root);
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
        json!({"hook_event_name": "PreToolUse", "tool_use_id": "t3", "tool_name": "Edit",
               "tool_input": {"file_path": "/src/hook.rs"}}),
        json!({"hook_event_name": "PostToolUse", "tool_use_id": "t3", "tool_name": "Edit",
               "duration_ms": 20}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m3", "index": 0, "delta": "fixed"}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "fixed"}),
    ];
    for mut event in turn {
        event["session_id"] = json!(session);
        event["cwd"] = json!(project(root));
        hook(root, &event);
        // What a turn says after a run of tool calls arrives once those calls have run,
        // which is far longer than the wait a call is filed after.
        std::thread::sleep(Duration::from_millis(400));
    }

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        holding(&made),
        [
            ("silent", ">run the tests"),
            (
                "silent",
                "on it\n\n\
                 × **Bash**  `cargo test` **4s**  \n⎿ Exit code 1  \n\
                 ● [Explore] **Read**  `/src/listen.rs` **12ms**"
            ),
            (
                "silent",
                "one test fails\n\n● **Edit**  `/src/hook.rs` **20ms**"
            ),
            ("ring", "fixed"),
        ]
        .map(|(sound, body)| (sound, body.to_owned()))
    );
    let sent: Vec<&Call> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage "))
        .collect();
    assert!(
        sent[1].markdown.starts_with("**project** `01234567/"),
        "the run's head reads {:?}",
        sent[1].markdown
    );
    assert_eq!(
        sent[1].reply,
        replying_to(sent[0].id),
        "the run threads under the prompt"
    );
    drop(daemon);
}

/// A run that would not fit under the words introducing it leaves them their message
/// and goes on in one of its own.
#[test]
fn a_run_outgrowing_the_words_it_follows_goes_on_in_its_own_message() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("parted", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let session = "0123456789abcdef";
    let long = "x".repeat(32_000);

    let turn = [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "go"}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0, "delta": long}),
        json!({"hook_event_name": "PreToolUse", "tool_use_id": "t1", "tool_name": "Bash",
               "tool_input": {"command": "cargo test"}}),
        json!({"hook_event_name": "PostToolUse", "tool_use_id": "t1", "duration_ms": 30}),
        json!({"hook_event_name": "PreToolUse", "tool_use_id": "t2", "tool_name": "Bash",
               "tool_input": {"command": "cargo build"}}),
        json!({"hook_event_name": "PostToolUse", "tool_use_id": "t2", "duration_ms": 40}),
        json!({"hook_event_name": "MessageDisplay", "message_id": "m2", "index": 0, "delta": "done"}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
    ];
    for mut event in turn {
        event["session_id"] = json!(session);
        event["cwd"] = json!(project(root));
        hook(root, &event);
        std::thread::sleep(Duration::from_millis(400));
    }

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        holding(&made),
        [
            ("silent", ">go".to_owned()),
            ("silent", long.clone()),
            (
                "silent",
                "● **Bash**  `cargo test` **30ms**  \n● **Bash**  `cargo build` **40ms**"
                    .to_owned()
            ),
            ("ring", "done".to_owned()),
        ]
    );
    drop(daemon);
}

/// An assistant message's last flush reaches the daemon after the hook of the tool
/// call that message ends with, so the words introducing a call are announced after it.
/// They belong above it in the chat all the same.
#[test]
fn a_call_announced_before_the_words_that_introduce_it_still_follows_them() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("settling", port);
    let root = temporary.path();
    let daemon = daemon(root);
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
        event["cwd"] = json!(project(root));
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
            (
                "silent",
                "let me check\n\n● **Bash**  `cargo test` **30ms**".to_owned()
            ),
            ("ring", "checked".to_owned()),
        ]
    );
    drop(daemon);
}

/// The three hook processes run at once, so a delta can land after the `Stop` of its own
/// turn. Opening a second turn for it would leave a draft beside the answer showing
/// something else, and carrying the status line under it.
#[test]
fn a_delta_landing_after_its_stop_opens_no_second_turn() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("straggler", port);
    let root = temporary.path();
    let daemon = daemon(root);
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
        event["cwd"] = json!(project(root));
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
    drop(daemon);
}

/// A message that replies to nothing still names a session: the one heard from last in
/// the chat it was sent in. These sessions run outside tmux, so what the daemon says
/// back names where the message went and the terminal that session is on. A reply to a
/// message that names no session goes nowhere.
#[test]
fn a_message_replying_to_nothing_goes_to_the_session_heard_from_last_in_its_chat() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("unaddressed", port);
    let root = temporary.path();
    let daemon = daemon(root);

    // The later session sorts first, so what answers is the one heard from last rather
    // than the first one the daemon happens to hold. The throwaway root is outside
    // `CHAT_PROJECTS`, so the session there, heard from last of all, is the private
    // chat's.
    for (session, cwd) in [
        ("fedcba9876543210", project(root).as_path()),
        ("0123456789abcdef", project(root).as_path()),
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
    drop(daemon);
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
    std::fs::create_dir_all(root.join("run/klaudo")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaudo/state.json"),
        json!([{
            "id": session,
            "dir": project(root),
            "pid": std::process::id(),
            "pane": null,
            "seen": seen,
            "trail": {"prompt": "", "last": [{"chat": GROUP, "topic": 77}, 5]},
        }])
        .to_string(),
    )
    .expect("the state");
    let daemon = daemon(root);

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
            "cwd": project(root),
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
    drop(daemon);
}

/// A session resumed in the terminal after it exited posts in the topic it last posted
/// in before it exited.
#[test]
fn a_resumed_session_returns_to_its_topic() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("resumed-topic", port);
    let root = temporary.path();
    let session = "0123456789abcdef";
    std::fs::create_dir_all(root.join("run/klaudo")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaudo/state.json"),
        json!([{
            "id": session,
            "dir": project(root),
            "pid": std::process::id(),
            "pane": null,
            "seen": 0,
            "trail": {"prompt": "", "last": [{"chat": GROUP, "topic": 77}, 5]},
        }])
        .to_string(),
    )
    .expect("the state");
    let daemon = daemon(root);

    for event in ["SessionEnd", "SessionStart"] {
        hook(
            root,
            &json!({"hook_event_name": event, "session_id": session, "cwd": project(root)}),
        );
    }
    hook(
        root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "cwd": project(root),
            "prompt": "from the terminal",
        }),
    );
    let made = collect(&calls, |call| call.label.starts_with("sendRichMessage"));
    let prompt = made
        .iter()
        .find(|call| call.label.starts_with("sendRichMessage"))
        .expect("the prompt");
    assert_eq!(prompt.body["message_thread_id"], json!(77));
    drop(daemon);
}

/// A session that has not posted yet goes to the topic of the session of its project
/// heard from last.
#[test]
fn a_new_session_takes_the_topic_of_its_project() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("project-topic", port);
    let root = temporary.path();
    std::fs::create_dir_all(root.join("run/klaudo")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaudo/state.json"),
        json!([{
            "id": "0123456789abcdef",
            "dir": project(root),
            "pid": std::process::id(),
            "pane": null,
            "seen": 0,
            "trail": {"prompt": "", "last": [{"chat": GROUP, "topic": 77}, 5]},
        }])
        .to_string(),
    )
    .expect("the state");
    let daemon = daemon(root);

    hook(
        root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "fedcba9876543210",
            "cwd": project(root),
            "prompt": "from a new terminal",
        }),
    );
    let made = collect(&calls, |call| call.label.starts_with("sendRichMessage"));
    let prompt = made
        .iter()
        .find(|call| call.label.starts_with("sendRichMessage"))
        .expect("the prompt");
    assert_eq!(prompt.body["message_thread_id"], json!(77));
    drop(daemon);
}

/// A conversation opened from a topic belongs to that topic from its first event, even
/// where another session of its project posted elsewhere.
#[test]
fn a_new_conversation_belongs_to_the_topic_it_was_asked_from() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("new-topic", port);
    let root = temporary.path();
    std::fs::create_dir_all(root.join("run/klaudo")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaudo/state.json"),
        json!([{
            "id": "0123456789abcdef",
            "dir": project(root),
            "pid": std::process::id(),
            "pane": null,
            "seen": 0,
            "trail": {"prompt": "", "last": [{"chat": GROUP, "topic": 77}, 5]},
        }])
        .to_string(),
    )
    .expect("the state");
    let daemon = daemon(root);
    let dir = project(root).display().to_string();

    chat.says_in(GROUP, 78, OWNER, &format!("/new {dir}"));
    collect(&calls, |call| {
        call.body["reply_markup"]["force_reply"] == json!(true)
    });
    chat.push(json!({
        "chat": {"id": GROUP},
        "from": {"id": OWNER},
        "text": "hello",
        "message_thread_id": 78,
        "is_topic_message": true,
        "reply_to_message": delivered(
            json!({
                "message_thread_id": 78,
                "is_topic_message": true,
                "rich_message": {"blocks": [
                    {"type": "paragraph", "text": [{"type": "code", "text": "new"}]},
                    {"type": "paragraph", "text": dir},
                ]},
            }),
            GROUP,
        ),
    }));
    let log = root.join("tmux.log");
    wait_for(
        || std::fs::read_to_string(&log).is_ok_and(|log| log.contains("new-session")),
        "the reply opened no window",
    );

    // Its first event comes before the prompt, as a trust dialog's does.
    hook(
        root,
        &json!({
            "hook_event_name": "Notification",
            "session_id": "fedcba9876543210",
            "cwd": project(root),
            "message": "Claude needs your permission",
        }),
    );
    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    let notice = made.last().expect("the notification");
    assert_eq!(notice.body["message_thread_id"], json!(78));
    drop(daemon);
}

/// A message outside every topic of a private chat in topic mode opens a topic of its
/// own, which reaches the session heard from last outside every topic, while a topic the
/// user named reaches no session outside it.
#[test]
fn a_topic_a_message_opened_reaches_the_sessions_outside_every_topic() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("implicit", port);
    let root = temporary.path();
    let seen = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis();
    // The session started in the terminal and has posted nothing yet.
    std::fs::create_dir_all(root.join("run/klaudo")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaudo/state.json"),
        json!([{
            "id": "0123456789abcdef",
            "dir": project(root),
            "pid": std::process::id(),
            "pane": null,
            "seen": seen,
            "trail": {"prompt": "", "last": null},
        }])
        .to_string(),
    )
    .expect("the state");
    let daemon = daemon(root);

    chat.says_in(GROUP, 78, OWNER, "anyone");
    chat.opens(GROUP, 79, OWNER, "anyone");
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
    assert_eq!(answered, [("no", Some(78)), ("`01234567`", Some(79))]);
    drop(daemon);
}

/// A message replying to nothing in a topic reaches the session at home there heard
/// from last, resumed once it has exited, ahead of a session outside every topic heard
/// from since.
#[test]
fn a_topic_resumes_its_latest_session() {
    let (port, _calls, chat) = recorder();
    let temporary = prepare("topic-resume", port);
    let root = temporary.path();
    let seen = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis();
    let (outside, exited) = ("0123456789abcdef", "fedcba9876543210");
    std::fs::create_dir_all(root.join("run/klaudo")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaudo/state.json"),
        json!([
            {
                "id": exited,
                "dir": project(root),
                "pid": std::process::id(),
                "pane": null,
                "seen": seen - 1000,
                "trail": {"prompt": "", "last": [{"chat": GROUP, "topic": 79}, 5]},
            },
            {
                "id": outside,
                "dir": project(root),
                "pid": std::process::id(),
                "pane": null,
                "seen": seen,
                "trail": {"prompt": "", "last": null},
            },
        ])
        .to_string(),
    )
    .expect("the state");
    let daemon = daemon(root);

    hook(
        root,
        &json!({"hook_event_name": "SessionEnd", "session_id": exited, "cwd": project(root)}),
    );
    chat.opens(GROUP, 79, OWNER, "carry on");
    let log = root.join("tmux.log");
    wait_for(
        || {
            std::fs::read_to_string(&log).is_ok_and(|log| {
                log.lines()
                    .any(|line| line.ends_with(&format!("--resume {exited}")))
            })
        },
        "the topic's exited session was not resumed",
    );
    drop(daemon);
}

/// A session killed mid-turn sends no event again, and the daemon still finds it gone:
/// the message that showed the turn running is rewritten to what the turn said.
#[test]
fn a_turn_whose_session_was_killed_stops_reading_as_running() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("killed", port);
    let root = temporary.path();
    let daemon = daemon(root);

    // Each hook's parent is the session, and every one of these shells exits once its
    // hook has.
    for event in [
        json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "0123456789abcdef",
            "cwd": project(root),
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
        passing.args(["-c", "\"$0\"; true", env!("CARGO_BIN_EXE_klaudo")]);
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
    drop(daemon);
}

/// A session idle through a restart of the daemon stays reachable, and so does one that
/// ended before it, though the daemon that heard them was killed with no chance to
/// write anything on its way out. The one that ended stays reachable after a reboot
/// too, which clears the runtime directory.
#[test]
fn what_the_daemon_knows_outlives_a_restart() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("restart", port);
    let root = temporary.path();
    let daemon = daemon(root);

    for (event, session, cwd) in [
        ("SessionStart", "0123456789abcdef", project(root).as_path()),
        ("SessionStart", "fedcba9876543210", root),
        ("SessionEnd", "fedcba9876543210", root),
    ] {
        hook(
            root,
            &json!({"hook_event_name": event, "session_id": session, "cwd": cwd}),
        );
    }
    // Answered once the events ahead of it are in, from a topic no session has run in.
    chat.says_in(OWNER, 5, OWNER, "anyone");
    collect(&calls, |call| {
        call.markdown.starts_with("no session has run here")
    });
    drop(daemon);
    std::fs::remove_file(root.join("run/klaudo/listen.sock")).expect("the old socket");
    let daemon = self::daemon(root);

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
    let resumed = || {
        std::fs::read_to_string(root.join("tmux.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.ends_with("--resume fedcba9876543210"))
            .count()
    };
    assert_eq!(resumed(), 1);
    drop(daemon);

    std::fs::remove_dir_all(root.join("run")).expect("the runtime directory");
    std::fs::create_dir(root.join("run")).expect("a fresh runtime directory");
    let daemon = self::daemon(root);
    chat.replies(
        OWNER,
        OWNER,
        "pick it up again",
        &json!({"rich_message": {"blocks": [
            {"type": "paragraph", "text": [{"type": "code", "text": "fedcba98"}]},
        ]}}),
    );
    wait_for(
        || resumed() == 2,
        "the exited session was forgotten at the reboot",
    );
    drop(daemon);
}

/// A reply to a session that has exited, whether its process is gone or it reported its
/// end, opens a window resuming it in the directory it ran in, and a second reply before
/// that session starts waits for the same window.
#[test]
fn a_reply_to_a_session_that_exited_resumes_it() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("resume", port);
    let root = temporary.path();
    let daemon = daemon(root);

    // The hook's parent is the session, and this shell exits once the hook has.
    let mut passing = within(root, "sh");
    passing.args(["-c", "\"$0\"; true", env!("CARGO_BIN_EXE_klaudo")]);
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
            {"type": "bold", "text": "project"}, " ", {"type": "code", "text": "01234567/89abcdef"},
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
        &json!({"caption": "project 77777777", "caption_entities": [
            {"type": "code", "offset": 8, "length": 8},
        ]}),
    );

    let made = collect(&calls, |call| call.chat == Some(OWNER));
    let said: Vec<_> = made
        .iter()
        .filter(|call| call.label.starts_with("sendRichMessage"))
        .map(|call| call.markdown.as_str())
        .collect();
    assert_eq!(said, ["`77777777` is not a session this daemon has seen"]);
    let log = std::fs::read_to_string(root.join("tmux.log")).expect("tmux was called");
    let windows: Vec<_> = log
        .lines()
        .filter(|line| line.starts_with("new-"))
        .collect();
    let window = |opening: &str, id: &str| {
        format!(
            "{opening} -P -F #{{pane_id}} #{{socket_path}} -c {} -n {} claude --resume {id}",
            root.display(),
            root.file_name().expect("a name").display()
        )
    };
    // The first window opens the session, which a later one joins.
    assert_eq!(
        windows,
        [
            window("new-session -d -s klaudo", "0123456789abcdef"),
            window("new-window -t =klaudo:", "fedcba9876543210"),
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
    drop(daemon);
}

/// `/new` alone offers the projects that ran in its chat, an exited one among them, led by
/// the project of the session the message would reach and then the one heard from last,
/// and a press on one rewrites the menu into the anchor for it.
#[test]
fn a_new_conversation_opens_in_a_project_picked_from_a_menu() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("menu", port);
    let root = temporary.path();
    let daemon = daemon(root);

    for dir in ["a", "b"] {
        std::fs::create_dir(root.join(dir)).expect("a project");
    }
    for (event, session, cwd) in [
        ("SessionStart", "aaaaaaaa", root.join("a")),
        ("SessionStart", "bbbbbbbb", root.join("b")),
        ("SessionStart", "cccccccc", project(root)),
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
    // Session a exited last, and a message replying to nothing reaches it.
    assert_eq!(labels, [a.clone(), b.clone()]);
    assert_eq!(menu.chat, Some(OWNER));

    let from_b =
        json!({"text": "bbbbbbbb", "entities": [{"type": "code", "offset": 0, "length": 8}]});
    chat.replies(OWNER, OWNER, "/new", &from_b);
    let made = collect(&calls, |call| call.label == "sendMessage");
    let menu = made.last().expect("the menu");
    assert_eq!(
        menu.body["reply_markup"]["inline_keyboard"][0][0]["text"],
        b.as_str()
    );

    chat.presses(OWNER, menu, "new 1");
    let made = collect(&calls, |call| call.label == "deleteMessage");
    let anchor = made
        .iter()
        .find(|call| call.label.starts_with("sendRichMessage"))
        .expect("the anchor");
    // The directory is escaped as prose, which the reader never sees.
    assert_eq!(
        anchor.markdown.replace('\\', ""),
        format!("**a** `new`\n\n{a}")
    );
    // The next message typed replies to the anchor.
    assert_eq!(
        anchor.body["reply_markup"],
        json!({"force_reply": true, "input_field_placeholder": format!("first prompt in {a}")
            .chars().take(64).collect::<String>()})
    );
    assert_eq!(
        made.last().expect("the menu taken back").target,
        Some(menu.id)
    );
    assert!(made.iter().any(|call| call.label == "answerCallbackQuery"));

    // The group's menu holds only the project posting there.
    chat.says(GROUP, OWNER, "/new@klaudo_bot");
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
    drop(daemon);
}

/// `/clear` posts the anchor of a new conversation in the project of the session a
/// message would reach, and with none, says so.
#[test]
fn a_new_conversation_opens_in_the_project_a_message_would_reach() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("clear", port);
    let root = temporary.path();
    let daemon = daemon(root);

    for dir in ["a", "b"] {
        std::fs::create_dir(root.join(dir)).expect("a project");
    }
    chat.says(OWNER, OWNER, "/clear");
    let made = collect(&calls, |call| call.label.starts_with("sendRichMessage"));
    assert!(
        made.last()
            .expect("the answer")
            .markdown
            .contains("no session has run here")
    );

    for (session, dir) in [("aaaaaaaa", "a"), ("bbbbbbbb", "b")] {
        hook(
            root,
            &json!({"hook_event_name": "SessionStart", "session_id": session, "cwd": root.join(dir)}),
        );
    }
    let anchored = |made: &[Call]| {
        let anchor = made.last().expect("the anchor");
        assert_eq!(anchor.body["reply_markup"]["force_reply"], true);
        anchor.markdown.replace('\\', "")
    };
    let a = root.join("a").display().to_string();
    let b = root.join("b").display().to_string();
    // Session b started last, and a message replying to nothing reaches it.
    chat.says(OWNER, OWNER, "/clear");
    let made = collect(&calls, |call| call.label.starts_with("sendRichMessage"));
    assert_eq!(anchored(&made), format!("**b** `new`\n\n{b}"));

    let from_a =
        json!({"text": "aaaaaaaa", "entities": [{"type": "code", "offset": 0, "length": 8}]});
    chat.replies(OWNER, OWNER, "/clear@klaudo_bot", &from_a);
    let made = collect(&calls, |call| call.label.starts_with("sendRichMessage"));
    assert_eq!(anchored(&made), format!("**a** `new`\n\n{a}"));
    drop(daemon);
}

/// `/resume` offers the sessions of the project a message would reach, the one heard from
/// last first, with a way back to the projects of its chat, and a press on a session
/// posts an anchor addressed to it that replies to the last message it left, taking the
/// menu back.
#[test]
fn a_conversation_is_resumed_from_a_menu_of_its_project() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("resume-menu", port);
    let root = temporary.path();
    let daemon = daemon(root);

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
    let sessions = made.last().expect("the sessions");
    assert_eq!(
        sessions.markdown,
        format!("Resume a conversation in {}:", root.display())
    );
    chat.presses(OWNER, sessions, "projects");
    let made = collect(&calls, |call| call.label == "editMessageText");
    let projects = made.last().expect("the projects");
    assert_eq!(projects.markdown, "Resume a conversation in:");
    chat.presses(OWNER, projects, "resume 0");
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
    let [(latest, _), (earlier, data), (_, "projects")] = buttons[..] else {
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
    drop(daemon);
}

/// The anchor `/resume` posts for `session` in topic `topic` of the group, picked from
/// the menu of the group's one project, and the calls made from the press on.
fn summon(chat: &Chat, calls: &Receiver<Call>, session: &str, topic: i64) -> (Call, Vec<Call>) {
    chat.says_in(GROUP, topic, OWNER, "/resume");
    let made = collect(calls, |call| call.label == "sendMessage");
    let projects = made.last().expect("the projects");
    chat.presses_in(OWNER, projects, "resume 0", Some(topic));
    let made = collect(calls, |call| call.label == "editMessageText");
    let sessions = made.last().expect("the sessions");
    chat.presses_in(OWNER, sessions, &format!("session {session}"), Some(topic));
    let made = collect(calls, |call| call.label == "deleteMessage");
    let anchor = made
        .iter()
        .position(|call| call.label.starts_with("sendRichMessage"))
        .expect("the anchor");
    (made[anchor].clone(), made)
}

/// The anchor `/resume` posts moves a session there, whether it is idle or has exited,
/// so its next turn from the terminal is posted under the anchor's topic.
#[test]
fn a_session_goes_where_resume_summons_it() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("summon", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let (idle, ended) = ("1111111111111111", "2222222222222222");
    for event in [
        json!({"hook_event_name": "UserPromptSubmit", "session_id": idle, "cwd": project(root), "prompt": "a"}),
        json!({"hook_event_name": "Stop", "session_id": idle, "last_assistant_message": "done a"}),
        json!({"hook_event_name": "UserPromptSubmit", "session_id": ended, "cwd": project(root), "prompt": "b"}),
        json!({"hook_event_name": "Stop", "session_id": ended, "last_assistant_message": "done b"}),
        json!({"hook_event_name": "SessionEnd", "session_id": ended, "cwd": project(root)}),
    ] {
        hook(root, &event);
    }
    collect(&calls, |call| call.markdown.ends_with("done b"));

    let anchors = [
        summon(&chat, &calls, idle, 78).0,
        summon(&chat, &calls, ended, 79).0,
    ];
    assert_eq!(anchors[0].body["message_thread_id"], json!(78));
    assert_eq!(anchors[1].body["message_thread_id"], json!(79));

    for (session, topic) in [(idle, 78), (ended, 79)] {
        for event in [
            json!({"hook_event_name": "SessionStart", "session_id": session, "cwd": project(root)}),
            json!({"hook_event_name": "UserPromptSubmit", "session_id": session, "cwd": project(root), "prompt": "again"}),
        ] {
            hook(root, &event);
        }
        let made = collect(&calls, |call| call.markdown.ends_with(">again"));
        let prompt = made.last().expect("the prompt");
        assert_eq!(prompt.body["message_thread_id"], json!(topic), "{session}");
    }
    drop(daemon);
}

/// A turn summoned while it runs goes on under the anchor: the message showing it moves
/// there, and the answer replies to the anchor, while the prompt stays where it was.
#[test]
fn a_running_turn_goes_on_where_resume_summons_it() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("summon-turn", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let session = "0123456789abcdef";
    hook(
        root,
        &json!({"hook_event_name": "UserPromptSubmit", "session_id": session,
            "cwd": project(root), "prompt": "work"}),
    );
    hook(
        root,
        &json!({"hook_event_name": "MessageDisplay", "session_id": session,
            "message_id": "m1", "index": 0, "delta": "working"}),
    );
    let made = collect(&calls, |call| call.markdown.contains("working"));
    let prompt = made
        .iter()
        .find(|call| call.markdown.ends_with(">work"))
        .expect("the prompt");
    let live = made
        .iter()
        .rfind(|call| {
            call.label.starts_with("sendRichMessage") && call.markdown.contains("working")
        })
        .expect("the turn on screen");
    assert_eq!(live.body["message_thread_id"], json!(null));

    let (anchor, made) = summon(&chat, &calls, session, 78);
    assert!(
        made.iter()
            .any(|call| call.label == "deleteMessage" && call.target == Some(live.id)),
        "the turn's message stays behind"
    );
    let moved = made
        .iter()
        .rfind(|call| {
            call.label.starts_with("sendRichMessage") && call.markdown.contains("working")
        })
        .expect("the turn on screen again");
    assert_eq!(moved.body["message_thread_id"], json!(78));
    assert_eq!(moved.reply, replying_to(anchor.id));

    hook(
        root,
        &json!({"hook_event_name": "Stop", "session_id": session,
            "last_assistant_message": "working"}),
    );
    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    let answer = made
        .iter()
        .find(|call| call.label == "sendRichMessage ring")
        .expect("the answer");
    assert_eq!(answer.body["message_thread_id"], json!(78));
    assert_eq!(answer.reply, replying_to(anchor.id));
    assert!(
        made.iter()
            .any(|call| call.label == "deleteMessage" && call.target == Some(moved.id)),
        "the moved message goes once the answer is in"
    );
    assert_eq!(prompt.body["message_thread_id"], json!(null));
    drop(daemon);
}

/// What the chat is left holding: every message Klaŭdo sent, in the order it sent them,
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
                held.get_mut(&target).expect("a message klaudo sent").1 = body(call);
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

/// One call the daemon made, as the server saw it.
#[derive(Clone)]
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
    arrived: Instant,
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
    assert!(title.starts_with("**project**"), "head reads {title:?}");
    let status = if text.is_empty() {
        body
    } else {
        body.strip_prefix(text)
            .and_then(|rest| rest.strip_prefix("\n\n"))
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

/// A window whose command exits before its session starts is answered in the chat, and
/// the next reply to that session opens another window rather than waiting for it.
#[test]
fn a_window_closed_before_its_session_started_is_reported() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("closed", port);
    let root = temporary.path();
    let daemon = daemon(root);

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
        {"type": "paragraph", "text": [{"type": "code", "text": "fedcba98"}]},
    ]}});
    std::fs::write(root.join("closed"), "").expect("the window closes");
    chat.replies(OWNER, OWNER, "pick it up", &replied);
    collect(&calls, |call| {
        call.chat == Some(OWNER)
            && call.markdown.starts_with("the window opened in ")
            && call
                .markdown
                .ends_with(" closed before its session started")
    });

    chat.replies(OWNER, OWNER, "once more", &replied);
    let log = root.join("tmux.log");
    wait_for(
        || {
            std::fs::read_to_string(&log)
                .is_ok_and(|log| log.lines().filter(|line| line.starts_with("new-")).count() == 2)
        },
        "the second reply opened no window",
    );
    drop(daemon);
}

/// A session in a window the daemon opened, unheard from for longer than `IDLE_HOURS`,
/// has its window closed, unless a command it started is still running.
#[test]
fn a_window_idle_for_long_is_closed_unless_a_command_still_runs() {
    let (port, _calls, _chat) = recorder();
    let temporary = prepare("idle", port);
    let root = temporary.path();
    let env = root.join("config/klaudo/env");
    let mut settings = std::fs::read_to_string(&env).expect("the settings");
    settings.push_str("IDLE_HOURS=1\n");
    std::fs::write(&env, settings).expect("the settings");
    let idle = Command::new("sleep")
        .arg("10")
        .spawn()
        .expect("an idle session");
    let busy = Command::new("sh")
        .args(["-c", "sleep 10 & wait"])
        .spawn()
        .expect("a session running a command");
    let hours_ago = now() * 1000 - 2 * 60 * 60 * 1000;
    let running = |id: &str, pid: u32, pane: &str| {
        json!({
            "id": id,
            "dir": project(root),
            "pid": pid,
            "pane": {"server": root.join("tmux.sock"), "id": pane},
            "seen": hours_ago,
            "trail": {"prompt": "", "last": null},
        })
    };
    std::fs::create_dir_all(root.join("run/klaudo")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaudo/state.json"),
        json!([
            running("0123456789abcdef", idle.id(), "%1"),
            running("fedcba9876543210", busy.id(), "%2"),
        ])
        .to_string(),
    )
    .expect("the state");
    let log = root.join("tmux.log");
    let daemon = daemon(root);
    wait_for(
        || std::fs::read_to_string(&log).is_ok_and(|log| log.contains("kill-pane -t %1")),
        "the idle window stayed open",
    );
    std::thread::sleep(GRACE);
    drop(daemon);
    let closed = std::fs::read_to_string(&log).expect("the log");
    for mut session in [idle, busy] {
        let _ = session.kill();
        let _ = session.wait();
    }
    assert_eq!(closed, "kill-pane -t %1\n");
}

/// A window the daemon has just typed a reply into stays open, though its session had
/// been idle until then and the prompt it submits has not been reported yet.
#[test]
fn a_window_just_typed_into_is_not_closed_for_idling() {
    let (port, _calls, chat) = recorder();
    let temporary = prepare("typed", port);
    let root = temporary.path();
    let env = root.join("config/klaudo/env");
    let mut settings = std::fs::read_to_string(&env).expect("the settings");
    settings.push_str("IDLE_HOURS=1\n");
    std::fs::write(&env, settings).expect("the settings");
    // The session runs on a terminal of its own, which the panes of the recording tmux
    // show, so a reply is typed into it.
    let session = Command::new("script")
        .args([
            "-qc",
            "tty > tty; echo $$ > pid; exec sleep 30",
            "/dev/null",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("a session on a terminal");
    let pid = root.join("pid");
    wait_for(
        || std::fs::read_to_string(&pid).is_ok_and(|pid| pid.ends_with('\n')),
        "the session never started",
    );
    let pid: u32 = std::fs::read_to_string(&pid)
        .expect("the pid")
        .trim()
        .parse()
        .expect("a pid");
    // Idle for just short of the hour when the daemon starts.
    let margin = 4000;
    std::fs::create_dir_all(root.join("run/klaudo")).expect("runtime directory");
    std::fs::write(
        root.join("run/klaudo/state.json"),
        json!([{
            "id": "0123456789abcdef",
            "dir": project(root),
            "pid": pid,
            "pane": {"server": root.join("tmux.sock"), "id": "%1"},
            "seen": now() * 1000 - 60 * 60 * 1000 + margin,
            "trail": {"prompt": "", "last": null},
        }])
        .to_string(),
    )
    .expect("the state");
    let started = Instant::now();
    let log = root.join("tmux.log");
    let daemon = daemon(root);
    chat.replies(
        OWNER,
        OWNER,
        "carry on",
        &json!({"rich_message": {"blocks": [
            {"type": "paragraph", "text": [{"type": "code", "text": "01234567"}]},
        ]}}),
    );
    wait_for(
        || std::fs::read_to_string(&log).is_ok_and(|log| log.contains("send-keys")),
        "the reply was never typed",
    );
    assert!(
        started.elapsed() < Duration::from_millis(margin),
        "the reply was typed after the window fell due"
    );
    std::thread::sleep(Duration::from_millis(margin).saturating_sub(started.elapsed()) + GRACE);
    drop(daemon);
    let typed = std::fs::read_to_string(&log).expect("the log");
    let mut session = session;
    let _ = session.kill();
    let _ = session.wait();
    assert!(!typed.contains("kill-pane"), "tmux was called with {typed}");
}

/// The stop button on the message showing a running turn sends its session Escape, and
/// the turn ends once its transcript shows it interrupted, keeping what it said. Until
/// then a second press sends nothing, a press the transcript never answers is reported,
/// and the button stays for another.
#[test]
fn a_turn_stopped_from_the_chat_ends_once_its_transcript_says_so() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("stop", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let interruption = json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]},
    });
    // An interruption of an earlier turn is in the transcript already.
    let transcript = root.join("transcript.jsonl");
    std::fs::write(&transcript, format!("{interruption}\n")).expect("the transcript");
    let mut session = talking(root, &transcript);

    let made = collect(&calls, |call| call.markdown.contains('✻'));
    let live = made.last().expect("the message showing the turn").clone();
    assert_eq!(
        live.body["reply_markup"]["inline_keyboard"],
        json!([[{"text": "Stop", "callback_data": "stop"}]])
    );
    let log = root.join("tmux.log");
    let escapes = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .matches("send-keys -t %1 Escape")
            .count()
    };
    chat.presses(OWNER, &live, "stop");
    chat.presses(OWNER, &live, "stop");
    let made = collect(&calls, |call| {
        call.markdown == "the turn did not stop within 5s of Escape"
    });
    assert_eq!(
        escapes(),
        1,
        "a press while the first was unanswered sent Escape"
    );
    assert_eq!(
        made.last().expect("the report").reply,
        replying_to(live.reply["message_id"].as_i64().expect("the prompt"))
    );

    chat.presses(OWNER, &live, "stop");
    wait_for(|| escapes() == 2, "the second press sent no Escape");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("the transcript");
    writeln!(file, "{interruption}").expect("the interruption");
    let made: Vec<Call> = collect(&calls, |call| call.markdown.ends_with("\n\nInterrupted"))
        .into_iter()
        .filter(|call| call.label != "answerCallbackQuery")
        .collect();
    let [sealed, answer] = &made[..] else {
        panic!(
            "the turn ended with {:?}",
            made.iter().map(|call| &call.label).collect::<Vec<_>>()
        );
    };
    // What the turn said stays in the message that showed it, without the button.
    assert_eq!(sealed.label, "editMessageText");
    assert_eq!(sealed.target, Some(live.id));
    assert!(
        sealed.markdown.ends_with("\n\ncounting"),
        "{:?}",
        sealed.markdown
    );
    assert_eq!(sealed.body["reply_markup"], json!(null));
    assert_eq!(answer.label, "sendRichMessage silent");
    assert!(
        answer
            .markdown
            .starts_with("**project** `01234567/aaaaaaaa`"),
        "{:?}",
        answer.markdown
    );
    assert!(
        answer.markdown.contains("#interrupted"),
        "{:?}",
        answer.markdown
    );
    assert_eq!(answer.reply, live.reply);

    chat.presses(OWNER, &live, "stop");
    collect(&calls, |call| {
        call.markdown == "that turn is no longer running"
    });
    assert_eq!(escapes(), 2);
    let _ = session.kill();
    let _ = session.wait();
    drop(daemon);
}

/// A session in pane `%1`, the shell `script` runs on a terminal of its own, which the
/// panes of the recording tmux show. Its hooks report from inside the pane that it
/// submitted a prompt whose transcript is at `transcript` and began answering, and it
/// stays running for half a minute.
fn talking(root: &Path, transcript: &Path) -> Child {
    let event = |event: serde_json::Value| {
        let mut event = event;
        event["session_id"] = json!("0123456789abcdef");
        event["cwd"] = json!(project(root));
        event["prompt_id"] = json!("aaaaaaaa-1111");
        event["transcript_path"] = json!(transcript);
        event.to_string()
    };
    std::fs::write(
        root.join("submit.json"),
        event(json!({"hook_event_name": "UserPromptSubmit", "prompt": "count the stars"})),
    )
    .expect("an event");
    std::fs::write(
        root.join("delta.json"),
        event(json!({"hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0, "delta": "counting"})),
    )
    .expect("an event");
    within(root, "script")
        .args([
            "-qc",
            "tty > tty; \"$KLAUDO\" < submit.json; \"$KLAUDO\" < delta.json; exec sleep 30",
            "/dev/null",
        ])
        .env("KLAUDO", env!("CARGO_BIN_EXE_klaudo"))
        .env("SHELL", "/bin/sh")
        .env("TMUX", format!("{},1,0", root.join("tmux.sock").display()))
        .env("TMUX_PANE", "%1")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("a session on a terminal")
}

/// A throwaway root holding the runtime directory the daemon binds its socket in and
/// the credentials file every `klaudo` process started from it reads, so a machine's
/// own credentials stay out of the test. It is removed when the test drops it, which a
/// failing test does too.
fn prepare(name: &str, port: u16) -> TempDir {
    let temporary = tempfile::Builder::new()
        .prefix(&format!("klaudo-{name}-"))
        .tempdir()
        .expect("throwaway root");
    let root = temporary.path();
    std::fs::create_dir_all(root.join("run")).expect("runtime directory");
    // A window the daemon opens goes to a `tmux` that records how it was called, so a
    // test never reaches the tmux server of the machine it runs on. Its session exists
    // once a `new-session` has been recorded, every pane is in it and shows the terminal
    // a `tty` file beside the log names, and its windows stay open until a `closed` file
    // appears there.
    std::fs::create_dir_all(root.join("bin")).expect("binary directory");
    let tmux = root.join("bin/tmux");
    std::fs::write(
        &tmux,
        r#"#!/bin/sh
dir="$(dirname "$0")/.."
log="$dir/tmux.log"
[ "$1" = -S ] && shift 2
case "$1" in
has-session) grep -q '^new-session' "$log" 2>/dev/null; exit ;;
display-message)
  [ -e "$dir/closed" ] && exit
  case "$*" in
  *session_name*) echo klaudo ;;
  *) cat "$dir/tty" 2>/dev/null || echo /dev/pts/0 ;;
  esac
  exit ;;
esac
printf '%s\n' "$*" >> "$log"
case "$1" in new-*) echo "%$(grep -c '^new-' "$log") $dir/tmux.sock" ;; esac
"#,
    )
    .expect("a recording tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755))
        .expect("an executable tmux");
    std::fs::create_dir(project(root)).expect("the project");
    std::fs::create_dir_all(root.join("config/klaudo")).expect("configuration directory");
    std::fs::write(
        root.join("config/klaudo/env"),
        format!("BOT_TOKEN=111111:secret\nCHAT_ID={GROUP}\nUSER_ID={OWNER}\nCHAT_PROJECTS={}\nAPI_BASE=http://127.0.0.1:{port}\n", project(root).display()),
    )
    .expect("credentials");
    temporary
}

/// The directory the sessions of a test run in, listed in `CHAT_PROJECTS`, so their
/// turns go to the group and their heads read `project`.
fn project(root: &Path) -> PathBuf {
    root.join("project")
}

/// The daemon, killed when the test drops it.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn daemon(root: &Path) -> Daemon {
    let child = klaudo(root).arg("listen").spawn().expect("run the daemon");
    let socket = root.join("run/klaudo/listen.sock");
    wait_for(|| socket.exists(), "the daemon never bound its socket");
    Daemon(child)
}

fn klaudo(root: &Path) -> Command {
    within(root, env!("CARGO_BIN_EXE_klaudo"))
}

/// Every directory the binary resolves what it needs from point into the throwaway
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
        .env("XDG_STATE_HOME", root.join("state"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .stdin(Stdio::piped());
    command
}

fn hook(root: &Path, event: &serde_json::Value) {
    report(klaudo(root), event);
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
    let mut command = klaudo(root);
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
        for (id, stream) in (1..).zip(listener.incoming()) {
            let stream = stream.expect("accept");
            let (sender, sending) = (sender.clone(), sending.clone());
            // A held poll answers on its own thread, so the calls behind it go through.
            std::thread::spawn(move || answer(stream, id, &sender, &sending));
        }
    });
    (port, receiver, chat)
}

/// Every update the chat has had, the first numbered 1. A poll hands over those from its
/// offset on, so an update the daemon never confirmed reaches the next daemon too.
#[derive(Clone, Default)]
struct Chat {
    updates: Arc<(Mutex<Vec<serde_json::Value>>, Condvar)>,
    /// Every chat action the daemon sent, with when it arrived. An action leaves nothing
    /// a later call acts on, so it stays out of the calls a turn made.
    actions: Arc<Mutex<Vec<(Instant, serde_json::Value)>>>,
}

impl Chat {
    /// A message from the phone, as Telegram delivers it. It replies to nothing, which
    /// is what makes it a message to route by itself.
    fn says(&self, chat: i64, sender: i64, text: &str) {
        self.replies(chat, sender, text, &serde_json::Value::Null);
    }

    /// A reply to `replied`, a message Klaŭdo posted as Telegram hands it back.
    fn replies(&self, chat: i64, sender: i64, text: &str, replied: &serde_json::Value) {
        let replied = if replied.is_null() {
            serde_json::Value::Null
        } else {
            delivered(replied.clone(), chat)
        };
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
        self.in_topic(chat, topic, sender, text, false);
    }

    /// A message sent outside every topic of a private chat in topic mode, which opens
    /// topic `topic` named after it.
    fn opens(&self, chat: i64, topic: i64, sender: i64, text: &str) {
        self.in_topic(chat, topic, sender, text, true);
    }

    fn in_topic(&self, chat: i64, topic: i64, sender: i64, text: &str, implicit: bool) {
        let mut opened = json!({"name": "a topic", "icon_color": 7_322_096});
        if implicit {
            opened["is_name_implicit"] = json!(true);
        }
        self.push(json!({
            "chat": {"id": chat},
            "from": {"id": sender},
            "text": text,
            "message_thread_id": topic,
            "is_topic_message": true,
            "reply_to_message": delivered(
                json!({
                    "message_id": topic,
                    "message_thread_id": topic,
                    "forum_topic_created": opened,
                }),
                chat,
            ),
        }));
    }

    fn push(&self, mut message: serde_json::Value) {
        message["message_id"] = json!(9000);
        message["date"] = json!(now());
        self.deliver(json!({"message": message}));
    }

    /// A press on a button of `menu`, a menu the daemon posted or rewrote, as Telegram
    /// hands it back.
    fn presses(&self, sender: i64, menu: &Call, data: &str) {
        self.presses_in(sender, menu, data, None);
    }

    /// A press on a button of `menu` where it stands in topic `topic`.
    fn presses_in(&self, sender: i64, menu: &Call, data: &str, topic: Option<i64>) {
        self.deliver(json!({
            "callback_query": {
                "id": "query",
                "from": {"id": sender},
                "message": {
                    "message_id": menu.target.unwrap_or(menu.id),
                    "date": now(),
                    "chat": {"id": menu.chat},
                    "text": menu.markdown,
                    "reply_markup": menu.body["reply_markup"],
                    "message_thread_id": topic,
                    "is_topic_message": topic.is_some(),
                },
                "data": data,
            },
        }));
    }

    fn deliver(&self, mut update: serde_json::Value) {
        let (updates, arrived) = &*self.updates;
        let mut updates = updates.lock().expect("the chat");
        update["update_id"] = json!(updates.len() + 1);
        updates.push(update);
        arrived.notify_all();
    }

    /// The updates from `offset` on, waiting up to `HOLD` for one to arrive.
    fn since(&self, offset: i64) -> Vec<serde_json::Value> {
        let skipped = usize::try_from(offset.max(1) - 1).expect("an offset");
        let (updates, arrived) = &*self.updates;
        let updates = updates.lock().expect("the chat");
        let (updates, _) = arrived
            .wait_timeout_while(updates, HOLD, |updates| updates.len() <= skipped)
            .expect("the chat");
        updates[skipped.min(updates.len())..].to_vec()
    }

    fn act(&self, body: serde_json::Value) {
        let mut actions = self.actions.lock().expect("the actions");
        actions.push((Instant::now(), body));
    }

    fn actions(&self) -> Vec<(Instant, serde_json::Value)> {
        self.actions.lock().expect("the actions").clone()
    }
}

/// `message` in `chat` with the fields Telegram puts on every message it delivers, where
/// the test left them out.
fn delivered(mut message: serde_json::Value, chat: i64) -> serde_json::Value {
    let fields = message.as_object_mut().expect("a message");
    fields.entry("message_id").or_insert(json!(1));
    fields.entry("date").or_insert(json!(now()));
    fields.entry("chat").or_insert(json!({"id": chat}));
    message
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs()
}

fn answer(mut stream: TcpStream, id: i64, calls: &Sender<Call>, chat: &Chat) {
    let mut received = Vec::new();
    let mut chunk = [0; 4096];
    let (method, length, boundary, head) = loop {
        let read = stream.read(&mut chunk).expect("request");
        // A daemon killed on its way to the next request leaves nothing to answer.
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
        // A photo's bytes read as text the same way on every run, which is all a test
        // compares them by.
        Some(boundary) => form(&String::from_utf8_lossy(&body), &boundary),
        None => serde_json::from_slice(&body).expect("a JSON body"),
    };

    // The poll that asks what the chat said is not a call the turn made.
    let sent = if method == "getUpdates" {
        let offset = body["offset"].as_i64().expect("an offset");
        json!({"ok": true, "result": chat.since(offset)}).to_string()
    } else if method == "sendChatAction" {
        chat.act(body);
        json!({"ok": true, "result": true}).to_string()
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
                arrived: Instant::now(),
            })
            .expect("record");
        let result = match method.as_str() {
            "setMyCommands" | "deleteMessage" | "setMessageReaction" | "answerCallbackQuery" => {
                json!(true)
            }
            _ => json!({"message_id": id}),
        };
        json!({"ok": true, "result": result}).to_string()
    };
    // A daemon killed while its poll was held has left nobody to answer.
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{sent}",
        sent.len()
    );
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
/// how `klaudo send` is called, and a file that is not there is named, both without a
/// panic.
#[test]
fn the_command_line_explains_itself() {
    let temporary = prepare("usage", 0);
    let root = temporary.path();
    let run = |args: &[&str]| klaudo(root).args(args).output().expect("run klaudo");

    for (asked, usage) in [
        (&["--help"][..], "Usage: klaudo [COMMAND]"),
        (&["-h"], "Usage: klaudo [COMMAND]"),
        (&["send", "--help"], "Usage: klaudo send <FILES>..."),
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
        String::from_utf8_lossy(&help.stdout).contains("\n\nUsage: klaudo send <FILES>...\n\n"),
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
        "klaudo: /definitely/not/here: No such file or directory (os error 2)\n"
    );

    // Nothing is listening in this root, which is what a stopped daemon looks like.
    let alone = run(&["send", path]);
    assert_eq!(alone.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&alone.stderr).starts_with("klaudo: the daemon at "));
}

/// The commands Klaŭdo answers are listed in the command menu of each chat, for the
/// user alone.
#[test]
fn the_commands_are_listed_for_the_user_in_both_chats() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("commands", port);
    let daemon = daemon(temporary.path());

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
    drop(daemon);
}

/// `/usage` answers from what the status lines last reported: the plan's limits from any
/// session, and the context of the session a message would reach, under that session's
/// head.
#[test]
fn usage_is_answered_from_the_status_lines() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("status", port);
    let root = temporary.path();
    let daemon = daemon(root);

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
        "/usage@klaudo_bot",
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

    drop(daemon);
}

/// `/diff` draws the unstaged changes of the session a message would reach, each file
/// folded under a line of what changed in it, and lists a file too large to draw.
#[test]
fn diff_draws_the_unstaged_changes_of_a_session() {
    let (port, calls, chat) = recorder();
    let temporary = prepare("diff", port);
    let root = temporary.path();
    let daemon = daemon(root);

    chat.says(OWNER, OWNER, "/diff");
    let made = collect(&calls, |call| call.label.starts_with("sendRichMessage"));
    assert_eq!(
        made.last().expect("the answer").markdown,
        "no session has run here to show the changes of"
    );

    let tree = root.join("a");
    std::fs::create_dir(&tree).expect("a project");
    let git = |args: &[&str]| {
        let ran = Command::new("git")
            .arg("-C")
            .arg(&tree)
            .args(args)
            .output()
            .expect("run git");
        assert!(ran.status.success(), "git {args:?}: {ran:?}");
    };
    git(&["init", "-q"]);
    let write = |name: &str, text: &str| std::fs::write(tree.join(name), text).expect("a file");
    write("a.rs", "fn main() {\n    let x = 1;\n}\n");
    write("big.lock", &"x\n".repeat(300));
    write("gone", "bye\n");
    git(&["add", "."]);
    write("a.rs", "fn main() {\n    let x = 2;\n}\n");
    write("big.lock", &"y\n".repeat(300));
    std::fs::remove_file(tree.join("gone")).expect("a deletion");
    // Staged alone, so it is no unstaged change.
    write("staged", "s\n");
    git(&["add", "staged"]);
    hook(
        root,
        &json!({"hook_event_name": "SessionStart", "session_id": "aaaaaaaa", "cwd": tree}),
    );

    chat.says(OWNER, OWNER, "/diff");
    let made = collect(&calls, |call| call.markdown.contains("tg://photo"));
    let posted = made.last().expect("the pictures");
    assert_eq!(posted.label, "sendRichMessage silent");
    assert_eq!(
        posted.markdown,
        "**a** `aaaaaaaa`\n\n\
         <details><summary>M `a.rs` \\+1 −1</summary>\n\n![](tg://photo?id=p0)\n\n</details>\n\n\
         M `big.lock` \\+300 −300, over the 500 lines drawn\n\n\
         <details><summary>D `gone` \\+0 −1</summary>\n\n![](tg://photo?id=p1)\n\n</details>"
    );
    assert_eq!(
        posted.body["rich_message"]["media"],
        json!([
            {"id": "p0", "media": {"type": "photo", "media": "attach://p0"}},
            {"id": "p1", "media": {"type": "photo", "media": "attach://p1"}},
        ])
    );
    for photo in ["p0", "p1"] {
        let bytes = posted.body[photo].as_str().expect("an uploaded photo");
        assert!(bytes.starts_with("\u{fffd}PNG\r\n"), "{photo} is no PNG");
    }
    assert_eq!(posted.reply["message_id"], 9000);
    drop(daemon);
}

/// The answer that closes a turn ends with what `/usage` answers, in one line of the
/// figures the status lines reported so far.
#[test]
fn a_turn_ends_with_a_status_line() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("line", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let turn = |prompt: &str, line: &str| {
        for mut event in [
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "go"}),
            json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
        ] {
            event["prompt_id"] = json!(prompt);
            event["session_id"] = json!("aaaaaaaa");
            event["cwd"] = json!(root);
            hook(root, &event);
        }
        let made = collect(&calls, |call| call.label == "sendRichMessage ring");
        let answer = &made.last().expect("the answer").markdown;
        assert!(answer.ends_with(line), "{answer}");
    };

    turn("p1", "done");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    status(
        root,
        &json!({
            "session_id": "aaaaaaaa",
            "context_window": {"context_window_size": 200_000, "current_usage": null, "used_percentage": null},
            "rate_limits": {
                "five_hour": {"used_percentage": 1, "resets_at": now + 3 * 3600 + 29 * 60 + 30},
                "seven_day": {"used_percentage": 56.4, "resets_at": now + 62 * 3600 + 30},
            },
        }),
    );
    turn("p2", "done\n\n`1% 3h30m · 56% 2d14h`");
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
        }),
    );
    // Limits a later status line leaves out are still the ones reported last.
    turn("p3", "done\n\n`5% 45.6k/1m · 1% 3h30m · 56% 2d14h`");
    drop(daemon);
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
/// turn has said, as an album in the order they were named, and `klaudo send` exits with
/// how the uploads went.
#[test]
fn a_file_a_turn_sends_lands_in_its_thread() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("file", port);
    let root = temporary.path();
    let daemon = daemon(root);
    let session = "0123456789abcdef";

    hook(
        root,
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "prompt_id": "aaaaaaaa-1111",
            "cwd": project(root),
            "prompt": "show me the log",
        }),
    );
    let file = root.join("build.log");
    std::fs::write(&file, "all green").expect("the file to send");
    let other = root.join("test.log");
    std::fs::write(&other, "all passed").expect("the file to send");
    let sent = klaudo(root)
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
        caption.starts_with("<b>project</b> <code>01234567/aaaaaaaa</code> "),
        "the caption reads {caption:?}"
    );
    assert_eq!(media[0]["parse_mode"], json!("HTML"));
    assert_eq!(media[1].get("caption"), None);

    // A session the daemon has never heard from has no thread to post in.
    let refused = klaudo(root)
        .args(["send".as_ref(), file.as_os_str()])
        .env("CLAUDE_CODE_SESSION_ID", "ffffffff")
        .output()
        .expect("run send");
    assert!(!refused.status.success(), "send to nowhere succeeded");
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        "klaudo: session ffffffff has not reported to klaudo\n"
    );

    // Run outside Claude Code, it names no session, and the directory it runs in is no
    // project listed for the group, so the file goes to the user's private chat.
    let bare = klaudo(root)
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
    drop(daemon);
}

/// A message's last flushes race the hook of the tool call that ends it, so a delta can
/// land after the daemon has already posted that message, carrying a paragraph rather
/// than a few characters. A tool reports late for the same reason, once the run holding
/// it is a message. Both belong in the message their segment became.
#[test]
fn a_flush_landing_after_its_message_was_posted_rewrites_that_message() {
    let (port, calls, _chat) = recorder();
    let temporary = prepare("straggling-flush", port);
    let root = temporary.path();
    let daemon = daemon(root);
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
        event["cwd"] = json!(project(root));
        hook(root, &event);
        std::thread::sleep(Duration::from_millis(300));
    }

    let made = collect(&calls, |call| call.label == "sendRichMessage ring");
    assert_eq!(
        holding(&made),
        [
            ("silent", ">go"),
            // The flush that lost the race is written into the message it belongs to, and
            // so is the outcome of a call that reported after that message went out.
            ("silent", "on it now\n\n● **Bash**  `cargo test` **30ms**"),
            ("ring", "done"),
        ]
        .map(|(sound, body)| (sound, body.to_owned()))
    );
    drop(daemon);
}

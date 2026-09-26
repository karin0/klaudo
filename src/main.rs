mod hook;
mod listen;
mod telegram;
mod tmux;

use std::io::Read;
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::parent_id;
use std::time::Duration;

use hook::Event;

/// The terminal holds back the text a delta carries until this process returns, so a
/// resident that has gone away must not turn into a stall.
const HANDOFF_TIMEOUT: Duration = Duration::from_millis(100);
/// Outlasts the resident's attempts at an upload, with room for the calls queued ahead
/// of it.
const UPLOAD_WAIT: Duration =
    Duration::from_secs(telegram::UPLOAD_TIMEOUT.as_secs() * telegram::ATTEMPTS as u64 + 60);
const USAGE: &str = "\
usage: klaude              read a Claude Code hook event on stdin
       klaude listen       run the resident that owns the chat
       klaude send <file>  post a file in the thread of this session's turn";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["-h" | "--help"] => println!("{USAGE}"),
        ["listen"] => listen::run(),
        ["send", file] => send(file),
        [] => {
            let mut raw = Vec::new();
            std::io::stdin().read_to_end(&mut raw).expect("hook input");
            let event: Event = serde_json::from_slice(&raw).expect("hook input is JSON");
            hook(&event, &raw);
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

/// A mistake in how the command was called, which is reported without a backtrace.
fn fail(message: &str) -> ! {
    eprintln!("klaude: {message}");
    std::process::exit(1);
}

fn hook(event: &Event, raw: &[u8]) {
    match event.hook_event_name.as_str() {
        // A subagent's text stays out of the chat.
        "MessageDisplay" if event.agent_id.is_some() => {}
        // Nothing here is a message on its own. `SessionStart` says the session is
        // ready for input, which is what a conversation opened from the chat waits for,
        // a tool call is a line of the run the resident is drafting, and a delta is a
        // fragment of the message the resident assembles from them. `PreToolUse` also
        // holds up the call it announces, so it must never reach the network.
        "SessionStart" | "MessageDisplay" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            forward(raw);
        }
        // Every other event ends up in the chat either way: through the resident, which
        // orders it against the draft, or from here when nothing is listening.
        _ => {
            if !forward(raw) {
                let directory = event
                    .directory()
                    .unwrap_or_else(|| std::path::PathBuf::from(&event.cwd));
                let head = hook::head(
                    &hook::project(&directory),
                    &event.session_id,
                    event.prompt_id.as_deref(),
                );
                // No resident reported this turn, so this message is all of it, and
                // there is no prompt of its own in the chat for it to reply to.
                let telegram = telegram::Telegram::new();
                telegram.send(
                    telegram.chat(&directory),
                    &hook::message(event, &head, ""),
                    telegram::Sound::Ring,
                    None,
                );
            }
        }
    }
}

/// Hands the event over with where its session lives. `$TMUX` and `$TMUX_PANE` are
/// inherited from that session, and `exec` in the hook command is what makes this
/// process a child of it, so the parent id is the session to type into.
fn forward(raw: &[u8]) -> bool {
    let Ok(socket) = UnixDatagram::unbound() else {
        return false;
    };
    let _ = socket.set_write_timeout(Some(HANDOFF_TIMEOUT));
    let context = serde_json::json!({
        "pid": parent_id(),
        "tmux": std::env::var("TMUX").ok(),
        "pane": std::env::var("TMUX_PANE").ok(),
    })
    .to_string();
    // Concatenated rather than re-serialised, because a delta's hook runs inside the
    // budget that holds the terminal's own output back.
    let mut handoff = Vec::with_capacity(context.len() + raw.len() + 10);
    handoff.extend_from_slice(context.trim_end_matches('}').as_bytes());
    handoff.extend_from_slice(b",\"event\":");
    handoff.extend_from_slice(raw);
    handoff.push(b'}');
    socket.send_to(&handoff, listen::socket_path()).is_ok()
}

/// Hands a file to the resident for the chat of the turn this command runs in, and waits
/// for how that went, so the exit status says whether the file reached the chat. Claude
/// Code names the session in the environment of every command it runs, and a command
/// run anywhere else sends to the chat of the directory it runs in.
fn send(file: &str) {
    let session = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
    let cwd = std::env::current_dir().expect("working directory");
    let file =
        std::fs::canonicalize(file).unwrap_or_else(|error| fail(&format!("{file}: {error}")));
    if !file.is_file() {
        fail(&format!("{} is not a file", file.display()));
    }
    let listening = listen::socket_path();
    // The resident creates the directory the reply socket is bound in.
    if !listening.exists() {
        fail(&format!(
            "the resident at {} is not running",
            listening.display()
        ));
    }
    let reply = listen::runtime_dir().join(format!("send-{}.sock", std::process::id()));
    let socket = UnixDatagram::bind(&reply).expect("bind");
    socket
        .set_read_timeout(Some(UPLOAD_WAIT))
        .expect("read timeout");
    let request = serde_json::json!({"session": session, "cwd": cwd, "file": file, "reply": reply});
    let mut answer = vec![0; 4096];
    let answered = socket
        .send_to(request.to_string().as_bytes(), &listening)
        .and_then(|_| socket.recv(&mut answer));
    let _ = std::fs::remove_file(&reply);
    let size = answered
        .unwrap_or_else(|error| fail(&format!("the resident at {}: {error}", listening.display())));
    if size > 0 {
        fail(&String::from_utf8_lossy(&answer[..size]));
    }
}

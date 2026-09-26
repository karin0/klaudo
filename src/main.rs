mod hook;
mod listen;
mod telegram;
mod tmux;

use std::io::Read;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::os::unix::process::parent_id;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use hook::Event;

/// The terminal holds back the text a delta carries until this process returns, so a
/// resident that has gone away must not turn into a stall.
const HANDOFF_TIMEOUT: Duration = Duration::from_millis(100);
/// Long enough for the Telegram calls queued ahead of the question, one retried after a
/// rejection included.
const LOCATE_WAIT: Duration = Duration::from_secs(60);

/// Carries a Claude Code session's turns to Telegram and what is typed there back into
/// its terminal. Without a command, it reads a hook event on stdin.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the resident that owns the chat
    Listen,
    /// Post a file in the thread of this session's turn
    Send {
        /// The file to post
        file: PathBuf,
    },
}

fn main() {
    match Cli::parse().command {
        Some(Command::Listen) => listen::run(),
        Some(Command::Send { file }) => send(&file),
        None => {
            let mut raw = Vec::new();
            std::io::stdin().read_to_end(&mut raw).expect("hook input");
            let event: Event = serde_json::from_slice(&raw).expect("hook input is JSON");
            hook(&event, &raw);
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
        // `SessionEnd` that a reply to it now resumes it, a tool call is a line of the
        // run the resident is drafting, and a delta is a fragment of the message the
        // resident assembles from them. `PreToolUse` also holds up the call it
        // announces, so it must never reach the network.
        "SessionStart" | "SessionEnd" | "MessageDisplay" | "PreToolUse" | "PostToolUse"
        | "PostToolUseFailure" => {
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
    let (Some(target), Ok(socket)) = (listen::socket_path(), UnixDatagram::unbound()) else {
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
    socket.send_to(&handoff, target).is_ok()
}

/// Asks the resident where a file goes and uploads it from here, so the upload holds up
/// nothing but this command, and the exit status says whether the file reached the chat.
/// Claude Code names the session in the environment of every command it runs, and a
/// command run anywhere else sends to the chat of the directory it runs in.
fn send(file: &Path) {
    let session = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
    let cwd = std::env::current_dir().expect("working directory");
    let file = std::fs::canonicalize(file)
        .unwrap_or_else(|error| fail(&format!("{}: {error}", file.display())));
    if !file.is_file() {
        fail(&format!("{} is not a file", file.display()));
    }
    let Some(listening) = listen::socket_path() else {
        fail("XDG_RUNTIME_DIR is not set, so there is no resident to reach");
    };
    let placement =
        locate(&listening, session.as_deref(), &cwd).unwrap_or_else(|error| fail(&error));
    let telegram = telegram::Telegram::new();
    let sent = telegram.document(
        placement.chat,
        &file,
        placement.caption.as_deref(),
        placement.reply_to,
    );
    // What Telegram answered is already on stderr.
    if sent.is_none() {
        std::process::exit(1);
    }
}

/// The answer arrives at an abstract address, which vanishes with this process however
/// it ends. Anyone on the machine can send to such an address, so only an answer from
/// the resident's own socket counts.
fn locate(
    listening: &Path,
    session: Option<&str>,
    cwd: &Path,
) -> Result<listen::Placement, String> {
    let unreachable = |error: std::io::Error| -> ! {
        fail(&format!("the resident at {}: {error}", listening.display()))
    };
    let name = format!("klaude-send-{}", std::process::id());
    let socket = SocketAddr::from_abstract_name(&name)
        .and_then(|address| UnixDatagram::bind_addr(&address))
        .expect("bind");
    let request = serde_json::json!({"session": session, "cwd": cwd, "reply": name});
    if let Err(error) = socket.send_to(request.to_string().as_bytes(), listening) {
        unreachable(error);
    }
    let deadline = Instant::now() + LOCATE_WAIT;
    let mut answer = vec![0; 64 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            fail(&format!(
                "the resident at {} did not answer within {}s",
                listening.display(),
                LOCATE_WAIT.as_secs()
            ));
        }
        socket.set_read_timeout(Some(left)).expect("read timeout");
        let (size, from) = match socket.recv_from(&mut answer) {
            Ok(received) => received,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => unreachable(error),
        };
        if from.as_pathname() == Some(listening) {
            return serde_json::from_slice(&answer[..size]).expect("the resident's answer");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer from any socket but the resident's is dropped, so a local user who
    /// guesses the address cannot say where the file goes.
    #[test]
    fn only_the_resident_answers_where_a_file_goes() {
        let directory = tempfile::tempdir().expect("a directory of the test's own");
        let listening = directory.path().join("listen.sock");
        let resident = UnixDatagram::bind(&listening).expect("bind the resident");
        std::thread::spawn(move || {
            let mut request = vec![0; 4096];
            let size = resident.recv(&mut request).expect("the question");
            let request: serde_json::Value =
                serde_json::from_slice(&request[..size]).expect("a JSON question");
            let reply =
                SocketAddr::from_abstract_name(request["reply"].as_str().expect("a reply address"))
                    .expect("an abstract address");
            let impostor = UnixDatagram::unbound().expect("socket");
            impostor
                .send_to_addr(br#"{"Err": "from someone else"}"#, &reply)
                .expect("the forged answer");
            resident
                .send_to_addr(
                    br#"{"Ok": {"chat": 7, "reply_to": 3, "caption": null}}"#,
                    &reply,
                )
                .expect("the answer");
        });

        let placement = locate(&listening, None, Path::new("/")).expect("the resident's answer");
        assert_eq!((placement.chat, placement.reply_to), (7, Some(3)));
    }
}

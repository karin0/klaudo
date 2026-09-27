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

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use hook::Event;

/// The terminal holds back the text a delta carries until this process returns, so a
/// resident that has gone away must not turn into a stall.
const HANDOFF_TIMEOUT: Duration = Duration::from_millis(100);
/// Long enough for the Telegram calls queued ahead of the question, one retried after a
/// rejection included.
const LOCATE_WAIT: Duration = Duration::from_secs(60);

/// Carries a Claude Code session's turns to Telegram and what is typed there back into
/// its terminal. `klaude send` is the one command to run by hand. Without a command, it
/// reads a hook event on stdin.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the resident that owns the chat
    Listen,
    /// Send files to the user, the one command to run by hand
    ///
    /// Up to ten files form one album. It exits with 0 once every file reached the user.
    /// Telegram takes files of up to 50 MB.
    Send {
        /// The files to post, in this order
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Forward the status line's input on stdin to the resident
    Status,
}

fn main() {
    match cli().command {
        Some(Command::Listen) => listen::run(),
        Some(Command::Send { files }) => send(&files),
        Some(Command::Status) => {
            if !hand_over(&wrapped("{\"status\":", &stdin())) {
                eprintln!("klaude: no resident is listening");
            }
        }
        None => {
            let raw = stdin();
            let event: Event = serde_json::from_slice(&raw).expect("hook input is JSON");
            hook(&event, &raw);
        }
    }
}

/// The arguments, parsed by a command line whose help ends with how `klaude send` is
/// called, since that is the command Claude has to learn, and whose list of commands
/// already has its first line.
fn cli() -> Cli {
    let mut command = Cli::command();
    command.build();
    let send = command
        .find_subcommand_mut("send")
        .expect("send is a subcommand");
    let usage = send.render_usage();
    let about = send.get_about().expect("send has an about").to_string();
    let long_about = send
        .get_long_about()
        .expect("send has a long about")
        .to_string();
    let details = long_about.trim_start_matches(&about).trim_start();
    let manual = format!("{usage}\n\n{details}");
    let matches = command.after_help(manual).get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

fn stdin() -> Vec<u8> {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).expect("stdin");
    raw
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
        // resident assembles from them, and `PreCompact` that a `/compact` the resident
        // typed was accepted. `PreToolUse` also holds up the call it announces, so it
        // must never reach the network.
        "SessionStart" | "SessionEnd" | "MessageDisplay" | "PreToolUse" | "PostToolUse"
        | "PostToolUseFailure" | "PreCompact" => {
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
    let context = serde_json::json!({
        "pid": parent_id(),
        "tmux": std::env::var("TMUX").ok(),
        "pane": std::env::var("TMUX_PANE").ok(),
    })
    .to_string();
    let head = format!("{},\"event\":", context.trim_end_matches('}'));
    hand_over(&wrapped(&head, raw))
}

/// `raw` as the last field of an object that `head` opens. Concatenated rather than
/// re-serialised, because a delta's hook runs inside the budget that holds the
/// terminal's own output back.
fn wrapped(head: &str, raw: &[u8]) -> Vec<u8> {
    let mut datagram = Vec::with_capacity(head.len() + raw.len() + 1);
    datagram.extend_from_slice(head.as_bytes());
    datagram.extend_from_slice(raw);
    datagram.push(b'}');
    datagram
}

fn hand_over(datagram: &[u8]) -> bool {
    let (Some(target), Ok(socket)) = (listen::socket_path(), UnixDatagram::unbound()) else {
        return false;
    };
    let _ = socket.set_write_timeout(Some(HANDOFF_TIMEOUT));
    socket.send_to(datagram, target).is_ok()
}

/// Asks the resident where the files go and uploads them from here, so the uploads hold
/// up nothing but this command, and the exit status says whether they reached the chat.
/// Claude Code names the session in the environment of every command it runs, and a
/// command run anywhere else sends to the chat of the directory it runs in. Every path is
/// checked before the first upload, so a mistyped one posts nothing.
fn send(files: &[PathBuf]) {
    let session = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
    let cwd = std::env::current_dir().expect("working directory");
    let files: Vec<PathBuf> = files
        .iter()
        .map(|file| {
            let resolved = std::fs::canonicalize(file)
                .unwrap_or_else(|error| fail(&format!("{}: {error}", file.display())));
            if !resolved.is_file() {
                fail(&format!("{} is not a file", resolved.display()));
            }
            resolved
        })
        .collect();
    let Some(listening) = listen::socket_path() else {
        fail("XDG_RUNTIME_DIR is not set, so there is no resident to reach");
    };
    let placement =
        locate(&listening, session.as_deref(), &cwd).unwrap_or_else(|error| fail(&error));
    let telegram = telegram::Telegram::new();
    let mut failed = false;
    for album in files.chunks(telegram::ALBUM) {
        let sent = telegram.documents(
            placement.chat,
            album,
            placement.caption.as_deref(),
            placement.reply_to,
        );
        // What Telegram answered is already on stderr.
        if sent.is_none() {
            for file in album {
                eprintln!("klaude: {} did not reach the chat", file.display());
            }
            failed = true;
        }
    }
    if failed {
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

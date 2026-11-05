mod diff;
mod hook;
mod listen;
mod process;
mod telegram;
mod tmux;

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::parent_id;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use hook::Event;

/// The terminal holds back the text a delta carries until this process returns, so a
/// daemon that has gone away must not turn into a stall.
const HANDOFF_TIMEOUT: Duration = Duration::from_millis(100);
/// Long enough for the Telegram calls queued ahead of the question, one retried after a
/// rejection included.
const LOCATE_WAIT: Duration = Duration::from_secs(60);

/// Klaŭdo posts a Claude Code session's turns to Telegram and types the replies sent
/// there into the session's terminal. `klaudo send` is the one command to run by hand.
/// Without a command, it reads a hook event on stdin.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon that sends and receives every Telegram message
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
    /// Forward the status line's input on stdin to the daemon
    Status,
}

fn main() {
    match cli().command {
        Some(Command::Listen) => listen::run(),
        Some(Command::Send { files }) => send(&files),
        Some(Command::Status) => {
            if !hand_over(wrapped("{\"status\":", &stdin())) {
                eprintln!("klaudo: no daemon is listening");
            }
        }
        None => {
            let raw = stdin();
            let event: Event = serde_json::from_slice(&raw).expect("hook input is JSON");
            hook(&event, &raw);
        }
    }
}

/// The arguments, parsed by a command line whose help ends with how `klaudo send` is
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
    eprintln!("klaudo: {message}");
    std::process::exit(1);
}

/// A base directory of the XDG specification, under `$HOME` when its variable is unset.
fn xdg_home(variable: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(variable) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(fallback),
    }
}

fn hook(event: &Event, raw: &[u8]) {
    match event.hook_event_name.as_str() {
        // A subagent's text stays out of the chat.
        "MessageDisplay" if event.agent_id.is_some() => {}
        // Nothing here is a message on its own. `SessionStart` says the session is
        // ready for input, which is what a conversation opened from the chat waits for,
        // `SessionEnd` that a reply to it now resumes it, a tool call is a line of the
        // run the daemon is drafting, and a delta is a fragment of the message the
        // daemon assembles from them, and `PreCompact` that a `/compact` the daemon
        // typed was accepted. `PreToolUse` also holds up the call it announces, so it
        // must never reach the network.
        "SessionStart" | "SessionEnd" | "MessageDisplay" | "PreToolUse" | "PostToolUse"
        | "PostToolUseFailure" | "PreCompact" => {
            forward(raw);
        }
        // Every other event ends up in the chat either way: through the daemon, which
        // orders it against the draft, or from here when nothing is listening.
        _ => {
            if !forward(raw) {
                let directory = event
                    .directory()
                    .unwrap_or_else(|| std::path::PathBuf::from(&event.cwd));
                let head =
                    hook::Head::new(&directory, &event.session_id, event.prompt_id.as_deref());
                // No daemon reported this turn, so this message is all of it, and
                // there is no prompt of its own in the chat for it to reply to.
                let telegram = telegram::Telegram::new();
                let place = telegram::Place {
                    chat: telegram.chat(&directory),
                    topic: None,
                };
                telegram.post(
                    place,
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
    hand_over(wrapped(&head, raw))
}

/// `raw` as the last field of an object that `head` opens. Concatenated rather than
/// re-serialised, because a delta's hook runs inside the budget that holds the
/// terminal's own output back.
fn wrapped(head: &str, raw: &[u8]) -> Vec<u8> {
    let mut request = Vec::with_capacity(head.len() + raw.len() + 1);
    request.extend_from_slice(head.as_bytes());
    request.extend_from_slice(raw);
    request.push(b'}');
    request
}

fn hand_over(request: Vec<u8>) -> bool {
    listen::socket_path().is_some_and(|target| ask(target, request, HANDOFF_TIMEOUT).is_ok())
}

/// Writes `request` to the daemon on a connection of its own and returns the connection,
/// where an answer may follow. Connecting waits while the daemon's backlog is full, so
/// the connection is made on a thread of its own that this process abandons after `wait`.
fn ask(target: PathBuf, request: Vec<u8>, wait: Duration) -> io::Result<UnixStream> {
    let (sent, connection) = channel();
    std::thread::spawn(move || {
        let connected = UnixStream::connect(target).and_then(|mut connection| {
            connection.write_all(&request)?;
            connection.shutdown(Shutdown::Write)?;
            Ok(connection)
        });
        // The caller stops listening once `wait` passes.
        let _ = sent.send(connected);
    });
    connection
        .recv_timeout(wait)
        .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

/// Asks the daemon where the files go and uploads them from here, so the uploads hold
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
        fail("XDG_RUNTIME_DIR is not set, so there is no daemon to reach");
    };
    let placement =
        locate(&listening, session.as_deref(), &cwd).unwrap_or_else(|error| fail(&error));
    let telegram = telegram::Telegram::new();
    let mut failed = false;
    for album in files.chunks(telegram::ALBUM) {
        let sent = telegram.documents(
            placement.place,
            album,
            placement.caption.as_deref(),
            placement.reply_to,
        );
        // What Telegram answered is already on stderr.
        if sent.is_none() {
            for file in album {
                eprintln!("klaudo: {} did not reach the chat", file.display());
            }
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}

/// The answer comes back on the connection the question went out on, which only the
/// daemon holds the other end of.
fn locate(
    listening: &Path,
    session: Option<&str>,
    cwd: &Path,
) -> Result<listen::Placement, String> {
    let unreachable = |error: io::Error| -> ! {
        fail(&format!("the daemon at {}: {error}", listening.display()))
    };
    let request = serde_json::json!({"locate": {"session": session, "cwd": cwd}});
    let mut connection = ask(
        listening.to_path_buf(),
        request.to_string().into_bytes(),
        LOCATE_WAIT,
    )
    .unwrap_or_else(|error| unreachable(error));
    connection
        .set_read_timeout(Some(LOCATE_WAIT))
        .expect("read timeout");
    let mut answer = Vec::new();
    match connection.read_to_end(&mut answer) {
        Ok(_) => serde_json::from_slice(&answer).expect("the daemon's answer"),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => fail(&format!(
            "the daemon at {} did not answer within {}s",
            listening.display(),
            LOCATE_WAIT.as_secs()
        )),
        Err(error) => unreachable(error),
    }
}

mod daemon;
mod hook;
mod telegram;

use std::fs::File;
use std::io::Read;
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use hook::Event;

/// The terminal holds back the text a delta carries until this process returns, so a
/// daemon that has gone away must not turn into a stall.
const HANDOFF_TIMEOUT: Duration = Duration::from_millis(100);

fn main() {
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        Some("daemon") => {
            let session = arguments.next().expect("daemon <session id> <cwd>");
            let cwd = arguments.next().expect("daemon <session id> <cwd>");
            daemon::run(&session, &cwd);
        }
        Some(unknown) => panic!("unknown argument {unknown}"),
        None => hook(),
    }
}

fn hook() {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).expect("hook input");
    let event: Event = serde_json::from_slice(&raw).expect("hook input is JSON");

    match event.hook_event_name.as_str() {
        // The daemon times the turn from its own start, so the prompt carries no payload.
        "UserPromptSubmit" => spawn(&event),
        // A subagent's text stays out of the chat.
        "MessageDisplay" => {
            if event.agent_id.is_none() {
                forward(&event, &raw);
            }
        }
        // Every other event ends up in the chat either way: through the daemon, which
        // orders it against the draft, or from here when no daemon is listening.
        _ => {
            if !forward(&event, &raw) {
                let head = hook::head(
                    &hook::project(&event.cwd),
                    &event.session_id,
                    event.prompt_id.as_deref(),
                );
                // No daemon reported this turn, so this message is all of it.
                telegram::Telegram::from_env()
                    .send(&hook::message(&event, &head, ""), telegram::Sound::Ring);
            }
        }
    }
}

fn forward(event: &Event, raw: &[u8]) -> bool {
    let Ok(socket) = UnixDatagram::unbound() else {
        return false;
    };
    let _ = socket.set_write_timeout(Some(HANDOFF_TIMEOUT));
    socket
        .send_to(raw, daemon::socket_path(&event.session_id))
        .is_ok()
}

// This process exits as soon as the daemon is on its feet, which hands the child to
// init; there is no parent left to reap it.
#[expect(
    clippy::zombie_processes,
    reason = "the hook exits before the daemon does"
)]
fn spawn(event: &Event) {
    std::fs::create_dir_all(daemon::runtime_dir()).expect("runtime directory");
    let log = File::options()
        .create(true)
        .append(true)
        .open(daemon::log_path(&event.session_id))
        .expect("daemon log");
    Command::new(std::env::current_exe().expect("own path"))
        .args(["daemon", &event.session_id, &event.cwd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // Out of the terminal's process group, so it outlives the keystroke that ends
        // the session it is still reporting.
        .process_group(0)
        .spawn()
        .expect("spawn daemon");
}

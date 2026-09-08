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

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("listen") => listen::run(),
        Some(unknown) => panic!("unknown argument {unknown}"),
        None => {
            let mut raw = Vec::new();
            std::io::stdin().read_to_end(&mut raw).expect("hook input");
            let event: Event = serde_json::from_slice(&raw).expect("hook input is JSON");
            hook(&event, &raw);
        }
    }
}

fn hook(event: &Event, raw: &[u8]) {
    match event.hook_event_name.as_str() {
        // A subagent's text stays out of the chat.
        "MessageDisplay" if event.agent_id.is_some() => {}
        // Nothing to post: this says the session is ready for input, which is what a
        // conversation opened from the chat waits for.
        "SessionStart" => {
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
                telegram::Telegram::from_env().send(
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

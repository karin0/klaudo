//! The one process per machine that owns every Telegram call of every session, and the
//! only reader of the chat. It holds the calls in order per turn, and it is where a
//! message from the chat becomes keystrokes in a session's terminal.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Error, ErrorKind};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use crate::hook::{self, Event};
use crate::telegram::{Sound, Telegram};
use crate::tmux::{self, Pane};

/// A draft disappears 30 seconds after its last frame, so a turn that goes quiet inside
/// a long tool call still needs frames to keep it on screen.
const REFRESH: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(200);
/// Long enough to let a restarting instance take over from one still shutting down.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(50);
/// After a rejected poll, before asking again.
const RETRY: Duration = Duration::from_secs(5);
/// The status words Claude Code cycles through while a turn is running.
const WORDS: &[&str] = &[
    "Baking",
    "Booping",
    "Brewing",
    "Churning",
    "Cogitating",
    "Computing",
    "Conjuring",
    "Considering",
    "Cooking",
    "Crafting",
    "Deliberating",
    "Determining",
    "Forging",
    "Herding",
    "Honking",
    "Hustling",
    "Ideating",
    "Inferring",
    "Marinating",
    "Moseying",
    "Mulling",
    "Musing",
    "Noodling",
    "Percolating",
    "Pondering",
    "Processing",
    "Puttering",
    "Reticulating",
    "Ruminating",
    "Schlepping",
    "Shucking",
    "Simmering",
    "Spinning",
    "Stewing",
    "Synthesizing",
    "Thinking",
    "Vibing",
];
/// A send past the socket's own limit fails, and the hook falls back to reporting the
/// event itself, so this only has to cover a clamped message with room to spare.
const DATAGRAM_MAX: usize = 200 * 1024;
/// What a message from the chat addresses when it opens a conversation rather than
/// continuing one.
const NEW: &str = "new";

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(base).join("klaude")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("listen.sock")
}

/// An event a hook forwarded, with where the session it came from lives. The server
/// and the pane belong together, so a session outside tmux carries neither.
#[derive(Deserialize)]
struct Handoff {
    pid: u32,
    #[serde(default)]
    tmux: Option<String>,
    #[serde(default)]
    pane: Option<String>,
    event: Event,
}

/// What reaches the socket, from a hook or from the poller reading the chat.
#[derive(Deserialize)]
#[serde(untagged)]
enum Arrival {
    Hook(Box<Handoff>),
    Chat { message: Value },
}

pub fn run() {
    let directory = runtime_dir();
    fs::create_dir_all(&directory).expect("runtime directory");
    let lock = File::create(directory.join("listen.lock")).expect("lock file");
    if !acquire(&lock) {
        return;
    }

    let path = socket_path();
    let _ = fs::remove_file(&path);
    let socket = UnixDatagram::bind(&path).expect("bind");
    socket.set_read_timeout(Some(POLL)).expect("read timeout");
    std::thread::spawn(|| poll(&socket_path()));

    let mut machine = Machine {
        telegram: Telegram::from_env(),
        sessions: BTreeMap::new(),
        opening: Vec::new(),
    };
    let mut buffer = vec![0u8; DATAGRAM_MAX];
    loop {
        match socket.recv(&mut buffer) {
            Ok(size) => match serde_json::from_slice(&buffer[..size]) {
                Ok(arrival) => machine.arrival(arrival),
                Err(error) => eprintln!("unreadable arrival: {error}"),
            },
            Err(error) if quiet(&error) => {}
            Err(error) => panic!("recv: {error}"),
        }
        machine.tick();
    }
}

fn quiet(error: &Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn acquire(lock: &File) -> bool {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return true,
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(error)) => panic!("lock: {error}"),
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(LOCK_RETRY);
    }
}

/// Reads the chat and hands each message to the process's own socket, so events from
/// the hooks and messages from the phone arrive through one queue in the order they
/// landed. Messages older than this loop are the backlog Telegram still holds, and
/// typing those into a terminal would replay an afternoon of asks.
fn poll(target: &Path) {
    let telegram = Telegram::from_env();
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let socket = UnixDatagram::unbound().expect("socket");
    let mut offset = 0;
    loop {
        let Some(updates) = telegram.updates(offset) else {
            std::thread::sleep(RETRY);
            continue;
        };
        for update in updates {
            if let Some(id) = update["update_id"].as_i64() {
                offset = id + 1;
            }
            let message = &update["message"];
            let sent = message["date"].as_i64().unwrap_or_default();
            if message.is_null() || sent < i64::try_from(started).unwrap_or(i64::MAX) {
                continue;
            }
            let handoff = serde_json::json!({"message": message}).to_string();
            if let Err(error) = socket.send_to(handoff.as_bytes(), target) {
                eprintln!("forward: {error}");
            }
        }
    }
}

/// One assistant message, streamed into a draft of its own until it is complete.
struct Segment {
    id: String,
    draft_id: i64,
    chunks: BTreeMap<u32, String>,
    framed: String,
    frame_at: Instant,
}

impl Segment {
    fn new(id: &str) -> Self {
        Self {
            draft_id: draft_id(id),
            id: id.to_owned(),
            chunks: BTreeMap::new(),
            framed: String::new(),
            frame_at: Instant::now(),
        }
    }

    fn text(&self) -> String {
        self.chunks.values().map(String::as_str).collect()
    }
}

/// One turn, from the event that started it to its Stop.
struct Turn {
    prompt_id: String,
    started: Instant,
    /// The message carrying what was asked, which the rest of the turn replies to.
    reply_to: Option<i64>,
    segment: Option<Segment>,
}

struct Session {
    project: String,
    cwd: PathBuf,
    pid: u32,
    pane: Option<Pane>,
    /// The messages of prompts that were submitted while a turn was running. Claude
    /// Code reports such a prompt under the running turn's id and only reveals its own
    /// when that turn begins, so the order they were submitted in is what pairs them.
    queued: VecDeque<Option<i64>>,
    turn: Option<Turn>,
}

impl Session {
    fn head(&self, id: &str, prompt: Option<&str>) -> String {
        hook::head(&self.project, id, prompt)
    }
}

struct Machine {
    telegram: Telegram,
    sessions: BTreeMap<String, Session>,
    /// Text from the chat waiting for the session whose window it opened, by directory.
    opening: Vec<(PathBuf, String)>,
}

impl Machine {
    fn arrival(&mut self, arrival: Arrival) {
        match arrival {
            Arrival::Hook(handoff) => {
                self.hook(handoff.pid, handoff.tmux.zip(handoff.pane), &handoff.event);
            }
            Arrival::Chat { message } => self.chat(&message),
        }
    }

    fn hook(&mut self, pid: u32, tmux: Option<(String, String)>, event: &Event) {
        let id = event.session_id.clone();
        let pane = tmux.map(|(server, pane)| Pane::new(&server, &pane));
        let session = self.sessions.entry(id.clone()).or_insert_with(|| Session {
            project: hook::project(&event.cwd),
            cwd: PathBuf::from(&event.cwd),
            pid,
            pane: pane.clone(),
            queued: VecDeque::new(),
            turn: None,
        });
        session.pid = pid;
        session.pane = pane;
        // Only the events that open a session carry where it is running.
        if !event.cwd.is_empty() {
            session.project = hook::project(&event.cwd);
            session.cwd = PathBuf::from(&event.cwd);
        }

        match event.hook_event_name.as_str() {
            "SessionStart" => self.started(&id),
            "UserPromptSubmit" => self.submitted(&id, event),
            "MessageDisplay" => self.delta(&id, event),
            "Stop" | "StopFailure" => self.finish(&id, event),
            _ => self.aside(&id, event),
        }
    }

    /// A session is ready for input once it says so, which is after the dialog that
    /// asks whether its folder is trusted. A conversation opened from the chat is
    /// waiting for exactly this to type its first prompt.
    fn started(&mut self, id: &str) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let Some(index) = self.opening.iter().position(|(cwd, _)| *cwd == session.cwd) else {
            return;
        };
        let (_, text) = self.opening.remove(index);
        self.send(id, &text);
    }

    fn submitted(&mut self, id: &str, event: &Event) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        // A prompt submitted with nothing running starts at once, so anything still
        // queued was cleared in the terminal and would mispair every later turn.
        let running = session.turn.is_some();
        if !running {
            session.queued.clear();
        }
        // A prompt that starts its turn at once reports that turn's id; a queued one
        // reports the running turn's, and its own is only revealed when it begins.
        let prompt = if running {
            None
        } else {
            event.prompt_id.as_deref()
        };
        let head = session.head(id, prompt);
        let message = hook::message(event, &head, "");
        // The phone's owner asked this, so it arrives without a sound.
        let posted = self.telegram.send(&message, Sound::Silent, None);
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        if running {
            session.queued.push_back(posted);
        } else {
            session.turn = Some(Turn {
                prompt_id: event.prompt_id.clone().unwrap_or_default(),
                started: Instant::now(),
                reply_to: posted,
                segment: None,
            });
        }
    }

    /// The turn an event belongs to, opening one when the event names a turn that has
    /// not been seen. That is how a queued prompt's turn begins: it is announced by the
    /// first event carrying its own id.
    fn turn(&mut self, id: &str, prompt_id: Option<&str>) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let named = prompt_id.unwrap_or_default();
        match &session.turn {
            Some(open) if open.prompt_id == named => return,
            Some(_) => self.seal(id),
            None => {}
        }
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        session.turn = Some(Turn {
            prompt_id: named.to_owned(),
            started: Instant::now(),
            reply_to: session.queued.pop_front().flatten(),
            segment: None,
        });
    }

    fn delta(&mut self, id: &str, event: &Event) {
        let (Some(message_id), Some(index), Some(delta)) =
            (&event.message_id, event.index, &event.delta)
        else {
            return;
        };
        self.turn(id, event.prompt_id.as_deref());
        let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
            return;
        };
        if turn
            .segment
            .as_ref()
            .is_none_or(|open| open.id != *message_id)
        {
            self.seal(id);
            let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
                return;
            };
            turn.segment = Some(Segment::new(message_id));
        }
        let Some(segment) = self
            .sessions
            .get_mut(id)
            .and_then(|s| s.turn.as_mut())
            .and_then(|t| t.segment.as_mut())
        else {
            return;
        };
        // Three hook processes run at once, so a delta can arrive ahead of its predecessor.
        segment.chunks.insert(index, delta.clone());
    }

    /// A segment that has stopped receiving text is complete, so what its draft was
    /// showing becomes a message. The turn interrupts once, at its end, so this is quiet.
    fn seal(&mut self, id: &str) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(turn) = session.turn.as_mut() else {
            return;
        };
        let Some(segment) = turn.segment.take() else {
            return;
        };
        let text = segment.text();
        if text.is_empty() {
            return;
        }
        let head = hook::head(&session.project, id, Some(&turn.prompt_id));
        // The tag marks a finished turn, and this segment is the middle of one.
        let posted = hook::compose(&head, &took(turn.started.elapsed()), "", &text);
        let reply_to = turn.reply_to;
        self.telegram.send(&posted, Sound::Silent, reply_to);
    }

    fn finish(&mut self, id: &str, event: &Event) {
        self.turn(id, event.prompt_id.as_deref());
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(turn) = session.turn.take() else {
            return;
        };
        // The last segment's text is what this event carries, so its draft is dropped
        // rather than posted a second time just above the message that repeats it.
        let head = hook::head(&session.project, id, Some(&turn.prompt_id));
        let message = hook::message(event, &head, &took(turn.started.elapsed()));
        // The one sound of the turn: the reply is complete and worth coming back to.
        self.telegram.send(&message, Sound::Ring, turn.reply_to);
    }

    /// Anything else a session reports lands in the thread of the turn it happened in.
    fn aside(&mut self, id: &str, event: &Event) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let prompt = session.turn.as_ref().map(|turn| turn.prompt_id.clone());
        let head = session.head(id, prompt.as_deref());
        let reply_to = session.turn.as_ref().and_then(|turn| turn.reply_to);
        let message = hook::message(event, &head, "");
        self.telegram.send(&message, Sound::Ring, reply_to);
    }

    fn tick(&mut self) {
        let gone: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| !Path::new(&format!("/proc/{}", session.pid)).exists())
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            // A session killed mid-turn never sends Stop, and what it did say still goes.
            self.seal(&id);
            self.sessions.remove(&id);
        }

        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            let Some(session) = self.sessions.get(&id) else {
                continue;
            };
            let Some(turn) = session.turn.as_ref() else {
                continue;
            };
            let Some(segment) = turn.segment.as_ref() else {
                continue;
            };
            let text = segment.text();
            if text == segment.framed && segment.frame_at.elapsed() < REFRESH {
                continue;
            }
            let head = hook::head(&session.project, &id, Some(&turn.prompt_id));
            let status = status(turn.started.elapsed(), segment.draft_id);
            let frame = format!("{head}\n\n{text}\n<tg-thinking>{status}</tg-thinking>");
            let draft = segment.draft_id;
            self.telegram.draft(draft, &frame);
            let Some(segment) = self
                .sessions
                .get_mut(&id)
                .and_then(|s| s.turn.as_mut())
                .and_then(|t| t.segment.as_mut())
            else {
                continue;
            };
            segment.framed = text;
            segment.frame_at = Instant::now();
        }
    }

    /// A message from the chat. What it replies to says where it goes: a message from a
    /// session reaches that session, and the message `/new` left behind opens a
    /// conversation in the directory it names.
    fn chat(&mut self, message: &Value) {
        let owner = self.telegram.chat();
        if message["chat"]["id"].as_i64() != Some(owner)
            || message["from"]["id"].as_i64() != Some(owner)
        {
            return;
        }
        let text = message["text"].as_str().unwrap_or_default().trim();
        if text.is_empty() {
            return;
        }
        if let Some(argument) = text.strip_prefix("/new") {
            self.anchor(argument.trim());
            return;
        }
        let replied = &message["reply_to_message"];
        match address(replied) {
            Some(address) if address == NEW => match body(replied) {
                Some(cwd) => self.open(Path::new(&cwd), text),
                None => self.say("that anchor names no directory"),
            },
            Some(address) => self.send(&address, text),
            None => self.say("reply to a message from the session you mean, or `/new <directory>`"),
        }
    }

    /// A message to reply to with the first prompt of a new conversation. Nothing is
    /// started yet, so an anchor left alone costs nothing.
    fn anchor(&mut self, argument: &str) {
        let Some(cwd) = expand(argument) else {
            self.say("`/new <directory>`");
            return;
        };
        if !cwd.is_dir() {
            self.say(&format!("`{}` is not a directory", cwd.display()));
            return;
        }
        let head = hook::head(&hook::project(&cwd.to_string_lossy()), NEW, None);
        let message = hook::compose(&head, "", "", &cwd.to_string_lossy());
        self.telegram.send(&message, Sound::Silent, None);
    }

    /// Opens a window for a conversation and keeps its first prompt until the session
    /// there reports that it is ready.
    fn open(&mut self, cwd: &Path, text: &str) {
        if let Err(error) = tmux::open(cwd) {
            self.say(&format!("tmux: {error}"));
            return;
        }
        self.opening.push((cwd.to_owned(), text.to_owned()));
    }

    /// Types into the session whose id starts with `address`.
    fn send(&mut self, address: &str, text: &str) {
        let Some((id, session)) = self.sessions.iter().find(|(id, _)| id.starts_with(address))
        else {
            self.say(&format!("`{address}` is not a session running here"));
            return;
        };
        let Some(pane) = session.pane.clone() else {
            self.say(&format!(
                "`{}` is not running in tmux",
                &id[..8.min(id.len())]
            ));
            return;
        };
        let pid = session.pid;
        if !pane.holds(pid) {
            // A terminal draws whatever it likes, so it goes in a fence rather than
            // through the markdown parser.
            let screen = pane
                .screen()
                .map(|screen| format!("\n\n```\n{screen}\n```"))
                .unwrap_or_default();
            self.say(&format!(
                "that terminal no longer holds the session{screen}"
            ));
            return;
        }
        if let Err(error) = pane.deliver(text) {
            self.say(&format!("tmux: {error}"));
        }
    }

    fn say(&self, text: &str) {
        self.telegram.send(text, Sound::Silent, None);
    }
}

/// The address in the head of a message klaude posted, which is the session it belongs
/// to or the anchor of a conversation that has not started.
fn address(message: &Value) -> Option<String> {
    let spans = paragraph(message, 0)?.as_array()?;
    let code = spans.iter().find(|span| span["type"] == "code")?;
    let session = plain(&code["text"]);
    let session = session.split('/').next()?;
    (!session.is_empty()).then(|| session.to_owned())
}

fn body(message: &Value) -> Option<String> {
    let body = plain(paragraph(message, 1)?);
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_owned())
}

/// A message klaude posted comes back as the blocks Telegram rendered its markdown
/// into, so its head and its body are the first two paragraphs of that.
fn paragraph(message: &Value, index: usize) -> Option<&Value> {
    let block = message["rich_message"]["blocks"].get(index)?;
    (block["type"] == "paragraph").then(|| &block["text"])
}

/// The text of a paragraph, or of one span of it, which Telegram writes as a bare
/// string wherever it carries no formatting of its own.
fn plain(node: &Value) -> String {
    match node {
        Value::String(text) => text.clone(),
        Value::Array(spans) => spans.iter().map(plain).collect(),
        Value::Object(_) => plain(&node["text"]),
        _ => String::new(),
    }
}

/// A directory as it would be typed in the chat, where a leading `~` is the only shell
/// spelling worth honouring because nothing here runs a shell.
fn expand(argument: &str) -> Option<PathBuf> {
    if argument.is_empty() {
        return None;
    }
    let path = match argument.strip_prefix('~') {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest.trim_start_matches('/')),
        None => PathBuf::from(argument),
    };
    path.canonicalize().ok()
}

/// The same across a segment's frames so they animate into each other, non-zero as
/// Telegram requires, and different per segment so the next draft replaces this one.
fn draft_id(message_id: &str) -> i64 {
    let mut hasher = DefaultHasher::new();
    message_id.hash(&mut hasher);
    i64::try_from(hasher.finish() & 0x7fff_ffff).expect("31 bits fit") | 1
}

/// What the draft says under the text it is streaming. The word changes once per
/// refresh and starts somewhere else in the list per segment, so a turn sitting in a
/// long tool call keeps showing a frame that differs from the last one.
fn status(elapsed: Duration, draft_id: i64) -> String {
    let step = draft_id.unsigned_abs() + elapsed.as_secs() / REFRESH.as_secs();
    let word =
        WORDS[usize::try_from(step).expect("a 31-bit id plus a turn's seconds") % WORDS.len()];
    format!("✻ {word}… ({})", took(elapsed).trim())
}

fn took(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!(" {seconds}s"),
        60..3600 => format!(" {}m{}s", seconds / 60, seconds % 60),
        _ => format!(" {}h{}m", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_time_reads_in_the_largest_unit_it_fills() {
        assert_eq!(took(Duration::from_secs(9)), " 9s");
        assert_eq!(took(Duration::from_secs(59)), " 59s");
        assert_eq!(took(Duration::from_secs(60)), " 1m0s");
        assert_eq!(took(Duration::from_secs(3599)), " 59m59s");
        assert_eq!(took(Duration::from_secs(3600)), " 1h0m");
        assert_eq!(took(Duration::from_secs(7860)), " 2h11m");
    }

    #[test]
    fn a_status_line_names_the_elapsed_time_and_moves_on_every_refresh() {
        let word = |seconds| {
            status(Duration::from_secs(seconds), 1)
                .split('…')
                .next()
                .expect("a word")
                .to_owned()
        };
        assert!(status(Duration::from_secs(80), 1).ends_with("… (1m20s)"));
        assert!(status(Duration::ZERO, 1).starts_with('✻'));
        assert_eq!(word(0), word(REFRESH.as_secs() - 1));
        assert_ne!(word(0), word(REFRESH.as_secs()));
        assert_ne!(status(Duration::ZERO, 1), status(Duration::ZERO, 2));
    }

    #[test]
    fn a_draft_id_is_stable_per_message_and_never_zero() {
        assert_eq!(draft_id("msg_1"), draft_id("msg_1"));
        assert_ne!(draft_id("msg_1"), draft_id("msg_2"));
        assert!(draft_id("").is_positive());
    }

    #[test]
    fn deltas_arriving_out_of_order_still_read_in_order() {
        let mut segment = Segment::new("msg_1");
        segment.chunks.insert(2, "third".to_owned());
        segment.chunks.insert(0, "first ".to_owned());
        segment.chunks.insert(1, "second ".to_owned());
        assert_eq!(segment.text(), "first second third");
    }

    /// A message klaude posted, as Telegram hands it back in the reply to it.
    fn posted(paragraphs: &[Value]) -> Value {
        let blocks: Vec<Value> = paragraphs
            .iter()
            .map(|text| serde_json::json!({"type": "paragraph", "text": text}))
            .collect();
        serde_json::json!({"rich_message": {"blocks": blocks}})
    }

    fn head_of(address: &str) -> Value {
        serde_json::json!([
            {"type": "bold", "text": "klaude"},
            " ",
            {"type": "code", "text": address},
            " 12s ",
            {"type": "hashtag", "text": "#claude", "hashtag": "claude"},
        ])
    }

    #[test]
    fn a_reply_is_addressed_by_the_head_of_the_message_it_answers() {
        let turn = posted(&[head_of("01234567/fedcba98"), "done".into()]);
        assert_eq!(address(&turn).as_deref(), Some("01234567"));
        let anchor = posted(&[head_of(NEW), "/home/u/p".into()]);
        assert_eq!(address(&anchor).as_deref(), Some(NEW));
        assert_eq!(address(&posted(&["plain".into()])), None);
        assert_eq!(
            address(&serde_json::json!({"text": "from the phone"})),
            None
        );
    }

    #[test]
    fn an_anchor_carries_its_directory_in_its_body() {
        assert_eq!(
            body(&posted(&[head_of(NEW), "/home/u/p".into()])).as_deref(),
            Some("/home/u/p")
        );
        // A path Telegram found something to format in still reads as its own text.
        let split = serde_json::json!(["/home/u/", {"type": "italic", "text": "p_q_r"}]);
        assert_eq!(
            body(&posted(&[head_of(NEW), split])).as_deref(),
            Some("/home/u/p_q_r")
        );
        assert_eq!(body(&posted(&[head_of(NEW)])), None);
    }

    #[test]
    fn a_directory_is_expanded_from_a_leading_tilde() {
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(expand("~").as_deref(), Some(Path::new(&home)));
        assert_eq!(expand(""), None);
        assert_eq!(expand("/definitely/not/here"), None);
    }
}

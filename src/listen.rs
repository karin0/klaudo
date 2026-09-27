//! The one process per machine that owns every Telegram call of every session, and the
//! only reader of the chat. It holds the calls in order per turn, and it is where a
//! message from the chat becomes keystrokes in a session's terminal.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hook::{self, Event};
use crate::telegram::{Place, Sound, Telegram};
use crate::tmux::{self, Pane};

/// The longest the message showing an open segment goes without its clock advancing,
/// so a turn that goes quiet inside a long tool call still reads as running.
const REFRESH: Duration = Duration::from_secs(30);
/// The shortest gap between two rewrites of that message. It is also how long a turn
/// runs before the message exists, so a turn answered at once leaves nothing to take
/// back.
const REWRITE: Duration = Duration::from_secs(3);
/// The shortest gap between two rewrites in a group. Telegram lets a bot send a group 20
/// messages a minute and counts a rewrite as one, so this leaves room for the rest of the
/// turn and for other sessions posting there.
const GROUP_REWRITE: Duration = Duration::from_secs(10);
/// How long a tool call waits before it is filed, so words arriving within that time
/// stand above it, and how long an open segment's text stays quiet before the message
/// showing it is rewritten, so a `Stop` arriving milliseconds behind its answer takes
/// over and the answer does not stand in the chat twice.
const SETTLE: Duration = Duration::from_millis(100);
/// How long a session killed mid-turn keeps its message showing the turn as running.
/// A session that exits on its own says so, and a message from the chat checks every
/// session before it is routed.
const SWEEP: Duration = Duration::from_secs(5);
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
const RESUME: &str = "resume";
const USAGE: &str = "usage";
/// What a button picking one session of a `/resume` menu carries ahead of its id.
const SESSION: &str = "session";
/// What the chat's command menu offers, each with the line it is listed under.
const COMMANDS: &[(&str, &str)] = &[
    (NEW, "Open a conversation in a directory"),
    (RESUME, "Resume a recent conversation"),
    (
        USAGE,
        "Show the plan's limits and the context of the conversation it replies to",
    ),
    ("compact", "Compact the conversation it replies to"),
];
/// How much of a session's latest prompt its button in a `/resume` menu shows.
const PROMPT_MAX: usize = 40;
/// How many exited sessions a reply can still resume, oldest forgotten first. An entry is
/// an id, a directory and the start of a prompt, so the list stays within a few hundred
/// kilobytes.
const ENDED_MAX: usize = 1000;
/// How many choices a menu offers, which a phone shows without scrolling.
const MENU_MAX: usize = 8;
/// How many calls a run lists before the oldest are counted instead.
const RUN_MAX: usize = 30;
/// The mark a call opens its line with, for how it went. Geometric shapes, which every
/// font draws as themselves; the circle Claude Code's own terminal uses is drawn as an
/// emoji here.
const RUNNING: char = '○';
const DONE: char = '●';
const FAILED: char = '×';
/// A line ends where the next call begins. Two spaces before the newline is what keeps
/// them apart, because a lone newline joins the lines into one paragraph.
const BREAK: &str = "  \n";
/// The first line of what a failed tool reported, past which it stops reading at a
/// glance.
const WHY_MAX: usize = 60;

/// Where the resident's socket lives. `$XDG_RUNTIME_DIR` belongs to this user alone, so
/// without it there is no resident to reach.
pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|base| PathBuf::from(base).join("klaude"))
}

pub fn socket_path() -> Option<PathBuf> {
    Some(runtime_dir()?.join("listen.sock"))
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

/// What reaches the socket, from a hook, from the poller reading the chat, or from
/// `klaude send` asking where a file goes. That question waits for the answer at the
/// abstract address `reply`, and names no session when it ran outside Claude Code, which
/// leaves `cwd` to say where the file goes.
#[derive(Deserialize)]
#[serde(untagged)]
enum Arrival {
    Hook(Box<Handoff>),
    Press {
        press: Press,
    },
    Status {
        status: Status,
    },
    Chat {
        message: Value,
    },
    Locate {
        session: Option<String>,
        cwd: PathBuf,
        reply: String,
    },
}

/// A button the chat pressed on a menu klaude posted. The menu is the message the button
/// hangs from, and `data` names the button.
#[derive(Deserialize)]
struct Press {
    id: String,
    from: Value,
    message: Value,
    data: String,
}

/// What Claude Code hands a session's status line, as `klaude status` forwards it. Both
/// parts are absent until the session's first API call returns.
#[derive(Deserialize)]
struct Status {
    session_id: String,
    context_window: Option<Window>,
    rate_limits: Option<Limits>,
}

#[derive(Deserialize, Clone)]
struct Window {
    context_window_size: u64,
    current_usage: Option<Usage>,
    used_percentage: Option<f64>,
}

/// The tokens the latest call sent, which is what the context holds.
#[derive(Deserialize, Clone)]
struct Usage {
    #[serde(rename = "input_tokens")]
    uncached: u64,
    #[serde(rename = "cache_creation_input_tokens")]
    cache_written: u64,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read: u64,
}

/// The plan's limits, which every session of the account reports alike.
#[derive(Deserialize)]
struct Limits {
    five_hour: Option<Limit>,
    seven_day: Option<Limit>,
}

#[derive(Deserialize)]
struct Limit {
    used_percentage: f64,
    /// Unix seconds.
    resets_at: u64,
}

/// Where the files `klaude send` uploads go, and the caption that addresses each of them.
#[derive(Serialize, Deserialize)]
pub struct Placement {
    pub place: Place,
    pub reply_to: Option<i64>,
    pub caption: Option<String>,
}

pub fn run() {
    let directory = runtime_dir().expect("XDG_RUNTIME_DIR");
    fs::create_dir_all(&directory).expect("runtime directory");
    let lock = File::create(directory.join("listen.lock")).expect("lock file");
    if !acquire(&lock) {
        return;
    }

    let path = directory.join("listen.sock");
    let _ = fs::remove_file(&path);
    let socket = UnixDatagram::bind(&path).expect("bind");
    let answers = socket.try_clone().expect("socket");
    std::thread::spawn(move || poll(&path));
    let arrivals = read(socket);

    let state = directory.join("state.json");
    let saved = load(&state);
    let mut machine = Machine {
        telegram: Telegram::new(),
        answers,
        sessions: saved
            .sessions
            .into_iter()
            .map(|known| {
                let mut session =
                    Session::new(known.dir, known.pid, known.pane, instant(known.seen));
                session.trail = known.trail;
                (known.id, session)
            })
            .collect(),
        opening: Vec::new(),
        ended: saved.ended,
        swept: Instant::now(),
        state,
        limits: None,
    };
    // Whatever exited while nothing was listening.
    machine.sweep();
    loop {
        let arrival = match machine.due() {
            Some(at) => arrivals.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => arrivals.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match arrival {
            Ok(arrival) => machine.arrival(arrival),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("the reader stopped"),
        }
        machine.tick();
    }
}

/// Moves every datagram out of the socket's buffer as it lands. A Telegram call holds
/// the machine for as long as the call takes, and the socket's own buffer is a few
/// hundred deltas deep, past which a hook's handoff fails and that hook posts its event
/// by itself, one message per delta.
fn read(socket: UnixDatagram) -> Receiver<Arrival> {
    let (sender, arrivals) = channel();
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; DATAGRAM_MAX];
        loop {
            let size = socket.recv(&mut buffer).expect("recv");
            match serde_json::from_slice(&buffer[..size]) {
                Ok(arrival) => sender.send(arrival).expect("the machine is running"),
                Err(error) => eprintln!("unreadable arrival: {error}"),
            }
        }
    });
    arrivals
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
    let telegram = Telegram::new();
    telegram.register(COMMANDS);
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
            // A press only redraws a menu or posts an anchor, so a backlog of them
            // replays nothing a terminal would take.
            let handoff = if update["callback_query"].is_object() {
                serde_json::json!({"press": update["callback_query"]})
            } else {
                let message = &update["message"];
                let sent = message["date"].as_i64().unwrap_or_default();
                if message.is_null() || sent < i64::try_from(started).unwrap_or(i64::MAX) {
                    continue;
                }
                serde_json::json!({"message": message})
            }
            .to_string();
            if let Err(error) = socket.send_to(handoff.as_bytes(), target) {
                eprintln!("forward: {error}");
            }
        }
    }
}

/// How a tool call went, and how long it took getting there.
enum Outcome {
    Running,
    Done(Duration),
    Failed(Duration, String),
}

/// One tool call, from the event that announced it to the one that said how it went.
struct Call {
    id: String,
    /// The subagent that made it, absent on the main thread.
    agent: Option<String>,
    name: String,
    /// What the call says it is doing, empty where its tool describes no call of itself.
    description: String,
    subject: String,
    outcome: Outcome,
}

impl Call {
    /// A mark for how it went, the tool and what it says it is doing, and the time it
    /// took. What it is working on and what a failure reported go on lines under that,
    /// where the eye finds them and a long command wraps without pushing the time away.
    /// The tool and the time are bold, so the eye finds a call and its cost down a run
    /// of lines whose middles are of every length.
    fn line(&self) -> String {
        let (mark, took, why) = match &self.outcome {
            Outcome::Running => (RUNNING, String::new(), None),
            Outcome::Done(took) => (DONE, format!(" **{}**", spent(*took)), None),
            Outcome::Failed(took, why) => (FAILED, format!(" **{}**", spent(*took)), Some(why)),
        };
        let agent = match &self.agent {
            Some(agent) => format!("[{agent}] "),
            None => String::new(),
        };
        // A description is prose, while what the call works on is a path or a command
        // and keeps the span that carries it verbatim. Two spaces after the tool, which
        // is what holds its name apart from the words that follow it.
        let (said, under) = match (self.description.as_str(), self.subject.as_str()) {
            ("", "") => (String::new(), None),
            ("", subject) => (format!("  {}", code(subject)), None),
            (description, "") => (format!("  {}", hook::prose(description)), None),
            (description, subject) => (
                format!("  {}", hook::prose(description)),
                Some(code(subject)),
            ),
        };
        let head = format!("{mark} {agent}**{}**{said}{took}", self.name);
        [
            Some(head),
            under.map(|under| format!("⎿ {under}")),
            why.map(|why| format!("⎿ {}", hook::prose(why))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(BREAK)
    }
}

/// A command reads as markdown where the message is markdown, so it travels as a code
/// span, whose backticks have to outlast any run of them the command carries.
fn code(subject: &str) -> String {
    let mut longest = 0;
    let mut ticks = 0;
    for character in subject.chars() {
        ticks = if character == '`' { ticks + 1 } else { 0 };
        longest = longest.max(ticks);
    }
    let fence = "`".repeat(longest + 1);
    // A span whose text opens or closes with a backtick needs the padding to keep it.
    match longest {
        0 => format!("{fence}{subject}{fence}"),
        _ => format!("{fence} {subject} {fence}"),
    }
}

/// What a stretch of a turn holds: an assistant message being streamed, or the run of
/// tool calls between two of them.
enum Body {
    /// Deltas by index, because three hook processes run at once and one can arrive
    /// ahead of its predecessor.
    Text(BTreeMap<u32, String>),
    Tools(Vec<Call>),
}

/// One stretch of a turn, written into the turn's open message until the next one
/// starts and that message is its own. A turn opens with an empty text segment, so what
/// stands in the chat while nothing has been said is the status line.
struct Segment {
    id: String,
    body: Body,
    /// What the open message was last written with and when, absent until it exists.
    written: Option<(String, Instant)>,
    /// The message this segment finished in and the elapsed time stamped on it, which
    /// is what a flush or an outcome arriving later rewrites.
    posted: Option<(i64, Duration)>,
    /// When the segment last received text.
    heard: Instant,
}

impl Segment {
    fn new(id: &str, body: Body) -> Self {
        Self {
            id: id.to_owned(),
            body,
            written: None,
            posted: None,
            heard: Instant::now(),
        }
    }

    fn text(&self) -> String {
        match &self.body {
            Body::Text(chunks) => chunks.values().map(String::as_str).collect(),
            Body::Tools(calls) => listing(calls),
        }
    }
}

/// A run of tool calls, a line per call. The oldest are counted rather than listed past
/// the cap, so what is running now stays in a message Telegram will take.
fn listing(calls: &[Call]) -> String {
    let elided = calls.len().saturating_sub(RUN_MAX);
    let head = (elided > 0).then(|| format!("… {elided} earlier"));
    head.into_iter()
        .chain(calls[elided..].iter().map(Call::line))
        .collect::<Vec<_>>()
        .join(BREAK)
}

/// The place a turn is posted in and the message there carrying what was asked, which
/// the rest of the turn replies to. A turn asked from the phone stays in the place it was
/// asked in, and any other goes to the session's home.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Thread {
    place: Place,
    prompt: Option<i64>,
}

/// Text from the chat and the message that carried it, which is the thread the turn it
/// becomes replies into.
struct Ask {
    text: String,
    place: Place,
    message: i64,
}

/// One turn, from the event that started it to its Stop.
struct Turn {
    prompt_id: String,
    /// Where this turn starts in the list of status words.
    seed: u64,
    started: Instant,
    thread: Thread,
    /// The message at the foot of the turn, carrying the open segment and the status
    /// line. The segment that finishes in it takes it, and the next one opens another.
    live: Option<i64>,
    segment: Option<Segment>,
    /// The segments the chat already has, kept because what belongs in one goes on
    /// arriving after it was posted.
    sealed: Vec<Segment>,
    /// Tool calls announced but not filed yet, oldest first. A call the turn ended on
    /// is dropped with the turn: the answer is what that message is for.
    pending: Vec<(Instant, Call)>,
}

impl Turn {
    /// When the message showing the open segment is due to be written next, which is
    /// only to move its clock while the chat already shows what the segment says.
    fn due(&self) -> Option<Instant> {
        let segment = self.segment.as_ref()?;
        let quiet = segment.heard + SETTLE;
        match &segment.written {
            None => Some((self.started + REWRITE).max(quiet)),
            Some((written, at)) if *written == segment.text() => Some(*at + REFRESH),
            Some((_, at)) => Some((*at + rewrite(self.thread.place.chat)).max(quiet)),
        }
    }

    /// True while the open segment is a run of tool calls rather than an assistant
    /// message.
    fn running_tools(&self) -> bool {
        matches!(&self.segment, Some(segment) if matches!(segment.body, Body::Tools(_)))
    }

    /// The run a tool call belongs to, which is the open one until the turn moves past
    /// it and the call reports from the chat.
    fn holding(&mut self, tool_use_id: &str) -> Option<&mut Segment> {
        self.segment
            .iter_mut()
            .chain(self.sealed.iter_mut())
            .find(|segment| match &segment.body {
                Body::Tools(calls) => calls.iter().any(|call| call.id == tool_use_id),
                Body::Text(_) => false,
            })
    }
}

struct Session {
    /// Where the session belongs, which is where it was opened rather than wherever a
    /// turn has since moved.
    dir: PathBuf,
    pid: u32,
    pane: Option<Pane>,
    /// The messages of prompts that were submitted while a turn was running. Claude
    /// Code reports such a prompt under the running turn's id and only reveals its own
    /// when that turn begins, so the order they were submitted in is what pairs them.
    queued: VecDeque<Thread>,
    /// What klaude has typed into this session and not yet seen reported as a prompt.
    asked: VecDeque<Ask>,
    turn: Option<Turn>,
    /// The turn that finished most recently, so its stragglers do not open it again.
    done: Option<String>,
    /// When this session was last heard from, which is what an unaddressed message from
    /// the chat is delivered by.
    seen: Instant,
    trail: Trail,
    /// The context its status line last reported, and when, in Unix seconds.
    window: Option<(Window, u64)>,
}

/// What a `/resume` menu shows of a session and leads back to: the start of its latest
/// prompt, and the last message it left in the chat as `(place, message)`.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Trail {
    prompt: String,
    last: Option<(Place, i64)>,
}

impl Session {
    fn new(dir: PathBuf, pid: u32, pane: Option<Pane>, seen: Instant) -> Self {
        Self {
            dir,
            pid,
            pane,
            queued: VecDeque::new(),
            asked: VecDeque::new(),
            turn: None,
            done: None,
            seen,
            trail: Trail::default(),
            window: None,
        }
    }

    fn left(&mut self, place: Place, message: Option<i64>) {
        if let Some(message) = message {
            self.trail.last = Some((place, message));
        }
    }

    /// Where the session posts between turns: its project's chat, in the topic its last
    /// message there went to, so a topic stays one conversation across the turns the
    /// terminal starts.
    fn home(&self, telegram: &Telegram) -> Place {
        let chat = telegram.chat(&self.dir);
        let topic = self
            .trail
            .last
            .and_then(|(place, _)| place.topic.filter(|_| place.chat == chat));
        Place { chat, topic }
    }

    fn head(&self, id: &str, prompt: Option<&str>) -> String {
        hook::head(&hook::project(&self.dir), id, prompt)
    }
}

struct Machine {
    telegram: Telegram,
    /// Where `klaude send` hears where its file goes.
    answers: UnixDatagram,
    sessions: BTreeMap<String, Session>,
    opening: Vec<Opening>,
    /// Every exited session, oldest first.
    ended: VecDeque<Ended>,
    swept: Instant,
    /// Where `Saved` is written.
    state: PathBuf,
    /// The limits a status line last reported, and when, in Unix seconds.
    limits: Option<(Limits, u64)>,
}

/// What of the resident outlives a restart, so a session idle through one is still
/// reachable: where each session is and when it was last heard from, and where each
/// exited one ran. A turn in flight and what the chat asked of a window are left behind.
#[derive(Serialize, Deserialize, Default)]
struct Saved {
    sessions: Vec<Known>,
    ended: VecDeque<Ended>,
}

/// A session that exited: where it ran, which is where a reply to it resumes it, and
/// when it was last heard from, which is how recent its project is.
#[derive(Serialize, Deserialize, Clone)]
struct Ended {
    id: String,
    dir: PathBuf,
    /// Unix milliseconds.
    seen: u64,
    trail: Trail,
}

#[derive(Serialize, Deserialize)]
struct Known {
    id: String,
    dir: PathBuf,
    pid: u32,
    pane: Option<Pane>,
    /// Unix milliseconds.
    seen: u64,
    trail: Trail,
}

/// A state file a different version wrote may not parse, and it only saves the sessions
/// from waiting for their next event, so that costs a warning.
fn load(path: &Path) -> Saved {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Saved::default(),
        Err(error) => panic!("{}: {error}", path.display()),
    };
    serde_json::from_slice(&raw).unwrap_or_else(|error| {
        eprintln!("{}: {error}", path.display());
        Saved::default()
    })
}

fn unix_millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).expect("a date before 2^64 ms")
    })
}

/// The moment `unix_millis` names, on this process's clock.
fn instant(millis: u64) -> Instant {
    let age = unix_millis(SystemTime::now()).saturating_sub(millis);
    let now = Instant::now();
    now.checked_sub(Duration::from_millis(age)).unwrap_or(now)
}

/// What the chat asked, waiting for the session of the window klaude opened for it.
struct Opening {
    dir: PathBuf,
    /// The session the window resumes, which is what the ask waits for. A new
    /// conversation waits for the next session to start in `dir`.
    resume: Option<String>,
    ask: Ask,
}

impl Machine {
    fn arrival(&mut self, arrival: Arrival) {
        match arrival {
            Arrival::Hook(handoff) => {
                self.hook(handoff.pid, handoff.tmux.zip(handoff.pane), &handoff.event);
            }
            Arrival::Press { press } => self.press(&press),
            Arrival::Status { status } => self.status(status),
            Arrival::Chat { message } => self.chat(&message),
            Arrival::Locate {
                session,
                cwd,
                reply,
            } => {
                let placement = self.locate(session.as_deref(), &cwd);
                let answer = serde_json::to_vec(&placement).expect("a placement serializes");
                let sent = SocketAddr::from_abstract_name(reply.as_bytes())
                    .and_then(|address| self.answers.send_to_addr(&answer, &address));
                if let Err(error) = sent {
                    eprintln!("answer {reply}: {error}");
                }
            }
        }
    }

    fn hook(&mut self, pid: u32, tmux: Option<(String, String)>, event: &Event) {
        let id = event.session_id.clone();
        let pane = tmux.map(|(server, pane)| Pane::new(&server, &pane));
        let directory = event.directory();
        if !self.sessions.contains_key(&id) {
            self.ended.retain(|ended| ended.id != id);
        }
        let session = self.sessions.entry(id.clone()).or_insert_with(|| {
            Session::new(PathBuf::from(&event.cwd), pid, pane.clone(), Instant::now())
        });
        session.pid = pid;
        session.pane = pane;
        session.seen = Instant::now();
        if let Some(directory) = directory {
            session.dir = directory;
        }

        match event.hook_event_name.as_str() {
            "SessionStart" => self.started(&id),
            "SessionEnd" => self.end(&id),
            "UserPromptSubmit" => self.submitted(&id, event),
            "MessageDisplay" => self.delta(&id, event),
            "PreToolUse" => self.calling(&id, event),
            "PostToolUse" | "PostToolUseFailure" => self.called(&id, event),
            "Stop" | "StopFailure" => self.finish(&id, event),
            "PreCompact" if event.manual() => self.compacting(&id),
            "PreCompact" => {}
            "PostCompact" if event.manual() => self.compacted(&id, event),
            // A session waiting on a dialog is the other thing worth coming back to.
            "Notification" => self.aside(&id, event, Sound::Ring),
            _ => self.aside(&id, event, Sound::Silent),
        }
        // Streamed text and tool calls arrive many times a second and change nothing
        // saved but `seen`, which the next event at a turn's edges saves.
        if !matches!(
            event.hook_event_name.as_str(),
            "MessageDisplay" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
        ) {
            self.save();
        }
    }

    fn save(&self) {
        let now = SystemTime::now();
        let saved = Saved {
            sessions: self
                .sessions
                .iter()
                .map(|(id, session)| Known {
                    id: id.clone(),
                    dir: session.dir.clone(),
                    pid: session.pid,
                    pane: session.pane.clone(),
                    seen: unix_millis(now - session.seen.elapsed()),
                    trail: session.trail.clone(),
                })
                .collect(),
            ended: self.ended.clone(),
        };
        // Renamed into place, so a resident killed mid-write leaves the last state whole.
        let written = self.state.with_extension("tmp");
        let raw = serde_json::to_vec(&saved).expect("the state serializes");
        if let Err(error) =
            fs::write(&written, raw).and_then(|()| fs::rename(&written, &self.state))
        {
            eprintln!("save {}: {error}", self.state.display());
        }
    }

    /// A session is ready for input once it says so, which is after the dialog that
    /// asks whether its folder is trusted. A conversation opened from the chat is
    /// waiting for exactly this to type its first prompt. A resumed session takes every
    /// ask that waited for it, while each ask that opened a new conversation had a
    /// window of its own.
    fn started(&mut self, id: &str) {
        let Some(dir) = self.sessions.get(id).map(|session| session.dir.clone()) else {
            return;
        };
        let mut asks: Vec<Ask> = self
            .opening
            .extract_if(.., |opening| opening.resume.as_deref() == Some(id))
            .map(|opening| opening.ask)
            .collect();
        if asks.is_empty()
            && let Some(index) = self
                .opening
                .iter()
                .position(|opening| opening.resume.is_none() && opening.dir == dir)
        {
            asks.push(self.opening.remove(index).ask);
        }
        for ask in asks {
            self.send(id, ask);
        }
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
        let typed = pair(
            &mut session.asked,
            event.prompt.as_deref().unwrap_or_default(),
        );
        // A prompt klaude typed is already in the chat as the message that asked for
        // it, and that message is what the turn replies to.
        let thread = if let Some(thread) = typed {
            if let Some(prompt) = thread.prompt {
                self.telegram.acknowledge(thread.place.chat, prompt);
            }
            thread
        } else {
            // A prompt that starts its turn at once reports that turn's id; a
            // queued one reports the running turn's, and its own is only revealed
            // when it begins.
            let prompt = if running {
                None
            } else {
                event.prompt_id.as_deref()
            };
            let head = session.head(id, prompt);
            let message = hook::message(event, &head, "");
            let place = session.home(&self.telegram);
            // The phone's owner asked this, so it arrives without a sound.
            Thread {
                place,
                prompt: self.telegram.send(place, &message, Sound::Silent, None),
            }
        };
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        session.trail.prompt = glimpse(event.prompt.as_deref().unwrap_or_default());
        session.left(thread.place, thread.prompt);
        if running {
            session.queued.push_back(thread);
        } else {
            let prompt_id = event.prompt_id.clone().unwrap_or_default();
            session.turn = Some(Turn {
                segment: Some(Segment::new(&prompt_id, Body::Text(BTreeMap::new()))),
                seed: seed(&prompt_id),
                prompt_id,
                started: Instant::now(),
                thread,
                live: None,
                sealed: Vec::new(),
                pending: Vec::new(),
            });
        }
    }

    /// Opens the turn an event belongs to when the event names one that has not been
    /// seen, which is how a queued prompt's turn begins. False for an event of a turn
    /// that already finished: the three hook processes run at once, so a delta can land
    /// after its own `Stop`, and opening a second turn for it would leave a message
    /// beside the answer showing something else.
    fn turn(&mut self, id: &str, prompt_id: Option<&str>) -> bool {
        let Some(session) = self.sessions.get(id) else {
            return false;
        };
        let named = prompt_id.unwrap_or_default();
        if session.done.as_deref() == Some(named) {
            return false;
        }
        match &session.turn {
            Some(open) if open.prompt_id == named => return true,
            Some(_) => self.seal(id),
            None => {}
        }
        let Some(session) = self.sessions.get_mut(id) else {
            return false;
        };
        let place = session.home(&self.telegram);
        session.turn = Some(Turn {
            prompt_id: named.to_owned(),
            seed: seed(named),
            started: Instant::now(),
            thread: session.queued.pop_front().unwrap_or(Thread {
                place,
                prompt: None,
            }),
            live: None,
            segment: Some(Segment::new(named, Body::Text(BTreeMap::new()))),
            sealed: Vec::new(),
            pending: Vec::new(),
        });
        true
    }

    fn delta(&mut self, id: &str, event: &Event) {
        let (Some(message_id), Some(index), Some(delta)) =
            (&event.message_id, event.index, &event.delta)
        else {
            return;
        };
        if !self.turn(id, event.prompt_id.as_deref()) {
            return;
        }
        let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
            return;
        };
        if let Some(segment) = turn
            .sealed
            .iter_mut()
            .find(|sealed| sealed.id == *message_id)
        {
            if let Body::Text(chunks) = &mut segment.body {
                chunks.insert(index, delta.clone());
            }
            self.amend(id, message_id);
            return;
        }
        if turn
            .segment
            .as_ref()
            .is_none_or(|open| open.id != *message_id)
        {
            self.seal(id);
            let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
                return;
            };
            turn.segment = Some(Segment::new(message_id, Body::Text(BTreeMap::new())));
        }
        let Some(segment) = self
            .sessions
            .get_mut(id)
            .and_then(|s| s.turn.as_mut())
            .and_then(|t| t.segment.as_mut())
        else {
            return;
        };
        if let Body::Text(chunks) = &mut segment.body {
            chunks.insert(index, delta.clone());
            segment.heard = Instant::now();
        }
    }

    /// A tool call waits out `SETTLE` before it is filed, so the words its own message
    /// ends with reach the chat first.
    fn calling(&mut self, id: &str, event: &Event) {
        let (Some(tool_use_id), Some(name)) = (&event.tool_use_id, &event.tool_name) else {
            return;
        };
        if !self.turn(id, event.prompt_id.as_deref()) {
            return;
        }
        let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
            return;
        };
        turn.pending.push((
            Instant::now(),
            Call {
                id: tool_use_id.clone(),
                agent: event.agent_type.clone(),
                name: name.clone(),
                description: event.description(),
                subject: event.subject(),
                outcome: Outcome::Running,
            },
        ));
    }

    /// Files the calls that have waited long enough. They join the run that is open,
    /// opening one when the turn was saying something instead. A run reads as one
    /// message, so a turn that talked, worked and talked again leaves those three in
    /// the chat in order.
    fn place(&mut self, id: &str) {
        let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
            return;
        };
        let ready = turn
            .pending
            .iter()
            .take_while(|(at, _)| at.elapsed() >= SETTLE)
            .count();
        if ready == 0 {
            return;
        }
        let filed: Vec<Call> = turn.pending.drain(..ready).map(|(_, call)| call).collect();
        if !turn.running_tools() {
            let opening = filed[0].id.clone();
            self.seal(id);
            let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
                return;
            };
            turn.segment = Some(Segment::new(&opening, Body::Tools(Vec::new())));
        }
        let Some(Body::Tools(calls)) = self
            .sessions
            .get_mut(id)
            .and_then(|s| s.turn.as_mut())
            .and_then(|t| t.segment.as_mut())
            .map(|segment| &mut segment.body)
        else {
            return;
        };
        calls.extend(filed);
    }

    /// How a call went, which reaches the run it belongs to. A run already posted takes
    /// no more outcomes, so a call it holds stays as it was when the turn moved on.
    fn called(&mut self, id: &str, event: &Event) {
        let Some(tool_use_id) = event.tool_use_id.as_deref() else {
            return;
        };
        let took = Duration::from_millis(event.duration_ms.unwrap_or_default());
        let outcome = if event.hook_event_name == "PostToolUseFailure" {
            Outcome::Failed(took, why(event.error.as_deref().unwrap_or("failed")))
        } else {
            Outcome::Done(took)
        };
        let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
            return;
        };
        // A tool this quick reports before its call has been filed.
        if let Some((_, call)) = turn
            .pending
            .iter_mut()
            .find(|(_, call)| call.id == tool_use_id)
        {
            call.outcome = outcome;
            return;
        }
        let Some(segment) = turn.holding(tool_use_id) else {
            return;
        };
        let Body::Tools(calls) = &mut segment.body else {
            return;
        };
        let Some(call) = calls.iter_mut().find(|call| call.id == tool_use_id) else {
            return;
        };
        call.outcome = outcome;
        let sealed = segment.posted.is_some().then(|| segment.id.clone());
        if let Some(segment_id) = sealed {
            self.amend(id, &segment_id);
        }
    }

    /// A segment that has stopped receiving text is complete, so it takes the open
    /// message and the elapsed time it finished at, and the next segment opens another.
    /// A segment that said nothing leaves the open message to the one that follows it.
    fn seal(&mut self, id: &str) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(turn) = session.turn.as_mut() else {
            return;
        };
        let Some(mut segment) = turn.segment.take() else {
            return;
        };
        let text = segment.text();
        if text.is_empty() {
            return;
        }
        let elapsed = turn.started.elapsed();
        let prompt_id = turn.prompt_id.clone();
        let thread = turn.thread;
        let live = turn.live.take();
        let head = session.head(id, Some(&prompt_id));
        // The tag marks a finished turn, and this segment is the middle of one.
        let done = hook::compose(&head, &took(elapsed), "", &text);
        let message = match live {
            Some(message) => {
                self.telegram.edit(thread.place.chat, message, &done);
                Some(message)
            }
            // A segment that ran its course inside one rewrite has no message yet.
            None => self
                .telegram
                .send(thread.place, &done, Sound::Silent, thread.prompt),
        };
        segment.posted = message.map(|message| (message, elapsed));
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        session.left(thread.place, message);
        let Some(turn) = session.turn.as_mut() else {
            return;
        };
        turn.sealed.push(segment);
    }

    /// A message klaude has posted, rewritten with what reached its segment afterwards.
    /// A message's last flushes race the hook of the tool call that ends it, and a tool
    /// reports after the run it belongs to has been left behind, so both land on a
    /// segment the chat already has.
    fn amend(&mut self, id: &str, segment_id: &str) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let Some(turn) = session.turn.as_ref() else {
            return;
        };
        let Some(segment) = turn.sealed.iter().find(|sealed| sealed.id == segment_id) else {
            return;
        };
        let Some((message, elapsed)) = segment.posted else {
            return;
        };
        let text = segment.text();
        let head = session.head(id, Some(&turn.prompt_id));
        self.telegram.edit(
            turn.thread.place.chat,
            message,
            &hook::compose(&head, &took(elapsed), "", &text),
        );
    }

    fn finish(&mut self, id: &str, event: &Event) {
        if !self.turn(id, event.prompt_id.as_deref()) {
            return;
        }
        // A run of tool calls is a message of its own, and only an assistant message is
        // what this event repeats.
        if self
            .sessions
            .get(id)
            .and_then(|s| s.turn.as_ref())
            .is_some_and(Turn::running_tools)
        {
            self.seal(id);
        }
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(turn) = session.turn.take() else {
            return;
        };
        session.done = Some(turn.prompt_id.clone());
        let head = session.head(id, Some(&turn.prompt_id));
        let mut message = hook::message(event, &head, &took(turn.started.elapsed()));
        if event.hook_event_name == "Stop"
            && let Some(line) = status_line(session.window.as_ref(), self.limits.as_ref())
        {
            message = format!("{message}\n\n{line}");
        }
        // The one sound of the turn: the reply is complete and worth coming back to.
        let thread = turn.thread;
        let answer = self
            .telegram
            .send(thread.place, &message, Sound::Ring, thread.prompt);
        session.left(thread.place, answer);
        // This event carries the last segment's text, so the message that was showing
        // it goes rather than standing above the one that repeats it.
        if let Some(live) = turn.live {
            self.telegram.delete(thread.place.chat, live);
        }
    }

    /// A `/compact` klaude typed never reports as a prompt, so its start is what tells
    /// the chat it was accepted.
    fn compacting(&self, id: &str) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        if let Some(ask) = compaction(&session.asked) {
            self.telegram.acknowledge(ask.place.chat, ask.message);
        }
    }

    /// `/compact` runs no turn, so its end is the answer to it and rings like one,
    /// replying to the message that asked for it when klaude typed it.
    fn compacted(&mut self, id: &str, event: &Event) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let typed = compaction(&session.asked)
            .map(|ask| ask.text.clone())
            .and_then(|text| pair(&mut session.asked, &text));
        let head = session.head(id, event.prompt_id.as_deref());
        let thread = typed.unwrap_or_else(|| self.thread(&self.sessions[id]));
        let answer = self.telegram.send(
            thread.place,
            &hook::message(event, &head, ""),
            Sound::Ring,
            thread.prompt,
        );
        if let Some(session) = self.sessions.get_mut(id) {
            session.left(thread.place, answer);
        }
    }

    /// Anything else a session reports lands in the thread of the turn it happened in.
    fn aside(&mut self, id: &str, event: &Event, sound: Sound) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let prompt = session.turn.as_ref().map(|turn| turn.prompt_id.clone());
        let head = session.head(id, prompt.as_deref());
        let thread = self.thread(session);
        let message = hook::message(event, &head, "");
        let posted = self
            .telegram
            .send(thread.place, &message, sound, thread.prompt);
        if let Some(session) = self.sessions.get_mut(id) {
            session.left(thread.place, posted);
        }
    }

    /// Where a file the session asks to show goes: the thread of its turn, below
    /// everything the turn has said so far, under the head a message of the turn carries,
    /// so a reply to the file reaches the session. A file sent from outside any session
    /// goes bare to the chat of the directory it was sent from.
    fn locate(&mut self, id: Option<&str>, cwd: &Path) -> Result<Placement, String> {
        let Some(id) = id else {
            return Ok(Placement {
                place: Place {
                    chat: self.telegram.chat(cwd),
                    topic: None,
                },
                reply_to: None,
                caption: None,
            });
        };
        if !self.sessions.contains_key(id) {
            return Err(format!("session {id} has not reported to klaude"));
        }
        self.seal(id);
        let session = &self.sessions[id];
        let turn = session.turn.as_ref();
        let address = hook::address(id, turn.map(|turn| turn.prompt_id.as_str()));
        let took = turn
            .map(|turn| took(turn.started.elapsed()))
            .unwrap_or_default();
        let caption = format!(
            "<b>{}</b> <code>{address}</code>{took}",
            hook::html(&hook::name(&session.dir))
        );
        let thread = self.thread(session);
        Ok(Placement {
            place: thread.place,
            reply_to: thread.prompt,
            caption: Some(caption),
        })
    }

    /// Where a session's messages go: its turn's thread, or its home between turns.
    fn thread(&self, session: &Session) -> Thread {
        session.turn.as_ref().map_or(
            Thread {
                place: session.home(&self.telegram),
                prompt: None,
            },
            |turn| turn.thread,
        )
    }

    /// The next moment `tick` has work: a tool call settling, a message falling due, or
    /// the sweep for a session killed mid-turn. With none, only an arrival wakes it.
    fn due(&self) -> Option<Instant> {
        let turns = || {
            self.sessions
                .values()
                .filter_map(|session| session.turn.as_ref())
        };
        let sweep = turns().next().map(|_| self.swept + SWEEP);
        turns()
            .flat_map(|turn| [turn.pending.first().map(|(at, _)| *at + SETTLE), turn.due()])
            .flatten()
            .chain(sweep)
            .min()
    }

    fn tick(&mut self) {
        if self.swept.elapsed() >= SWEEP {
            self.sweep();
        }
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            self.place(&id);
            self.show(&id);
        }
    }

    fn sweep(&mut self) {
        self.swept = Instant::now();
        let gone: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| !Path::new(&format!("/proc/{}", session.pid)).exists())
            .map(|(id, _)| id.clone())
            .collect();
        if gone.is_empty() {
            return;
        }
        for id in gone {
            self.end(&id);
        }
        self.save();
    }

    /// A session that ended mid-turn never sends Stop, and what it did say still goes.
    /// Its process may linger after `SessionEnd`, or it may have been killed before
    /// saying anything, so both that event and the sweep end a session here.
    fn end(&mut self, id: &str) {
        self.seal(id);
        let live = self
            .sessions
            .get_mut(id)
            .and_then(|session| session.turn.as_mut())
            .and_then(|turn| Some((turn.thread.place.chat, turn.live.take()?)));
        if let Some((chat, message)) = live {
            self.telegram.delete(chat, message);
        }
        let session = self.sessions.remove(id).expect("a session that ended");
        self.ended.push_back(Ended {
            id: id.to_owned(),
            dir: session.dir,
            seen: unix_millis(SystemTime::now() - session.seen.elapsed()),
            trail: session.trail,
        });
        if self.ended.len() > ENDED_MAX {
            self.ended.pop_front();
        }
    }

    /// The open segment as the chat should be showing it: what it has said so far and
    /// the status line under that. The message holding it is sent once the turn has run
    /// long enough to be worth watching, and rewritten as the segment grows.
    fn show(&mut self, id: &str) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let Some(turn) = session.turn.as_ref() else {
            return;
        };
        if turn.due().is_none_or(|at| at > Instant::now()) {
            return;
        }
        let elapsed = turn.started.elapsed();
        let text = turn.segment.as_ref().expect("a segment falling due").text();
        let head = session.head(id, Some(&turn.prompt_id));
        let live = turn.live;
        let thread = turn.thread;
        let shown = hook::compose(
            &head,
            &took(elapsed),
            "",
            &running(&text, &status(elapsed, turn.seed)),
        );
        let message = match live {
            Some(message) => {
                self.telegram.edit(thread.place.chat, message, &shown);
                Some(message)
            }
            None => self
                .telegram
                .send(thread.place, &shown, Sound::Silent, thread.prompt),
        };
        let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
            return;
        };
        turn.live = message;
        if message.is_some()
            && let Some(segment) = turn.segment.as_mut()
        {
            segment.written = Some((text, Instant::now()));
        }
    }

    /// A message from the chat. What it replies to says where it goes: a message from a
    /// session reaches that session, and the message `/new` left behind opens a
    /// conversation in the directory it names. A message replying to nothing goes to
    /// the session heard from last in its place.
    fn chat(&mut self, message: &Value) {
        let Some(place) = self.admitted(&message["from"], message) else {
            return;
        };
        let text = message["text"].as_str().unwrap_or_default().trim();
        if text.is_empty() {
            return;
        }
        let Some(carrier) = message["message_id"].as_i64() else {
            return;
        };
        let replied = &message["reply_to_message"];
        match command(text) {
            Some((NEW, "")) => {
                self.menu(place, NEW, "Open a conversation in:");
                return;
            }
            Some((NEW, argument)) => {
                self.anchor(place, argument);
                return;
            }
            Some((RESUME, _)) => {
                self.menu(place, RESUME, "Resume a conversation in:");
                return;
            }
            Some((USAGE, _)) => {
                self.usage(place, carrier, replied);
                return;
            }
            _ => {}
        }
        let ask = Ask {
            text: unaddressed(text),
            place,
            message: carrier,
        };
        // A session that exited without saying so is resumed rather than typed into.
        self.sweep();
        match self.addressee(place, replied) {
            Ok(Some(address)) if address == NEW => match body(replied) {
                Some(cwd) => self.open(PathBuf::from(cwd), None, ask),
                None => self.say(place, "that anchor names no directory"),
            },
            Ok(Some(address)) => self.send(&address, ask),
            Ok(None) => self.say(
                place,
                "no session is running here; `/new <directory>` opens one",
            ),
            Err(error) => self.say(place, error),
        }
    }

    /// Where a message goes: the session or anchor the message it replies to names, or
    /// for a message replying to nothing, the session heard from last in its place. A
    /// message outside every topic of a private chat in topic mode opens a topic whose
    /// name is implicit, so a topic like that holding no session reaches the sessions
    /// outside every topic, where the turns started in the terminal are posted.
    fn addressee(&self, place: Place, replied: &Value) -> Result<Option<String>, &'static str> {
        // A message in a topic that replies to nothing replies to the service message
        // that opened the topic.
        let opened = &replied["forum_topic_created"];
        if replied.is_null() || opened.is_object() {
            let outside = (opened["is_name_implicit"] == Value::Bool(true)).then_some(Place {
                topic: None,
                ..place
            });
            return Ok(self
                .latest(place)
                .or_else(|| outside.and_then(|outside| self.latest(outside))));
        }
        address(replied)
            .map(Some)
            .ok_or("the message replied to names no session")
    }

    /// Where a message came from, when it is one to act on.
    fn admitted(&self, sender: &Value, message: &Value) -> Option<Place> {
        let place = self.telegram.accepts(sender, message);
        if place.is_none() {
            // The ids to put in the env file are read from here.
            let chat = &message["chat"];
            eprintln!(
                "ignored: chat {} {:?} from {}",
                chat["id"],
                chat["title"].as_str().unwrap_or_default(),
                sender["id"]
            );
        }
        place
    }

    /// A button pressed on a menu. Its label is what it picks, so a menu posted before a
    /// restart still works.
    fn press(&mut self, press: &Press) {
        self.telegram.answer(&press.id);
        let Some(place) = self.admitted(&press.from, &press.message) else {
            return;
        };
        let Some(menu) = press.message["message_id"].as_i64() else {
            return;
        };
        let Some(label) = label(&press.message, &press.data) else {
            return;
        };
        match press.data.split_once(' ') {
            // An anchor opens the reply box only as it arrives, so it is a message of
            // its own, and a menu left behind is one mistaken press from a second one.
            Some((NEW, _)) => {
                if self.anchor(place, &label).is_some() {
                    self.telegram.delete(place.chat, menu);
                }
            }
            Some((RESUME, _)) => self.conversations(place, menu, &label),
            Some((SESSION, id)) => {
                if self.resumption(place, id).is_some() {
                    self.telegram.delete(place.chat, menu);
                }
            }
            _ => eprintln!("press: {}", press.data),
        }
    }

    /// The directories the sessions of `chat` ran in, the one heard from last first.
    fn projects(&self, chat: i64) -> Vec<PathBuf> {
        let now = SystemTime::now();
        let mut seen: Vec<(u64, &PathBuf)> = self
            .sessions
            .values()
            .map(|session| (unix_millis(now - session.seen.elapsed()), &session.dir))
            .chain(self.ended.iter().map(|ended| (ended.seen, &ended.dir)))
            .filter(|(_, dir)| self.telegram.chat(dir) == chat && dir.is_dir())
            .collect();
        seen.sort_by_key(|(seen, _)| std::cmp::Reverse(*seen));
        let mut projects: Vec<PathBuf> = Vec::new();
        for (_, dir) in seen {
            if !projects.contains(dir) {
                projects.push(dir.clone());
            }
        }
        projects.truncate(MENU_MAX);
        projects
    }

    /// Every session the resident knows of, running or exited, with where it ran, when
    /// it was last heard from in Unix milliseconds, and its trail.
    fn known(&self) -> impl Iterator<Item = (&str, &Path, u64, &Trail)> {
        let now = SystemTime::now();
        self.sessions
            .iter()
            .map(move |(id, session)| {
                let seen = unix_millis(now - session.seen.elapsed());
                (id.as_str(), session.dir.as_path(), seen, &session.trail)
            })
            .chain(self.ended.iter().map(|ended| {
                (
                    ended.id.as_str(),
                    ended.dir.as_path(),
                    ended.seen,
                    &ended.trail,
                )
            }))
    }

    /// The menu of a project's sessions a `/resume` menu leads to once the project is
    /// picked, rewritten over it.
    fn conversations(&self, place: Place, menu: i64, label: &str) {
        let Some(dir) = expand(label) else {
            self.say(place, &format!("{} is not a directory", code(label)));
            return;
        };
        let now = unix_millis(SystemTime::now());
        let mut sessions: Vec<_> = self.known().filter(|(_, ran, _, _)| *ran == dir).collect();
        sessions.sort_by_key(|(_, _, seen, _)| std::cmp::Reverse(*seen));
        let buttons: Vec<(String, String)> = sessions
            .iter()
            .take(MENU_MAX)
            .map(|(id, _, seen, trail)| {
                let age = ago(Duration::from_millis(now.saturating_sub(*seen)));
                let short = hook::address(id, None);
                let label = match trail.prompt.as_str() {
                    "" => format!("{short} · {age}"),
                    prompt => format!("{short} · {age} · {prompt}"),
                };
                (label, format!("{SESSION} {id}"))
            })
            .collect();
        if buttons.is_empty() {
            self.say(place, &format!("no session has run in {}", code(label)));
            return;
        }
        self.telegram.remenu(
            place.chat,
            menu,
            &format!("Resume a conversation in {label}:"),
            &buttons,
        );
    }

    /// An anchor addressed to session `id`, replying to the last message it left in this
    /// place, which a tap on the quotation scrolls back to. A reply to the anchor goes
    /// where a reply to any of its messages would.
    fn resumption(&self, place: Place, id: &str) -> Option<i64> {
        let Some((_, dir, _, trail)) = self.known().find(|(known, _, _, _)| *known == id) else {
            self.say(
                place,
                &format!("{} is not a session this resident has seen", code(id)),
            );
            return None;
        };
        let head = hook::head(&hook::project(dir), id, None);
        let message = hook::compose(&head, "", "", &hook::prose(&tilde(dir)));
        let last = trail
            .last
            .and_then(|(posted, message)| (posted == place).then_some(message));
        let placeholder = format!("prompt for {}", hook::address(id, None));
        self.telegram.anchor(place, &message, &placeholder, last)
    }

    /// A menu of the projects of the chat `place` is in, each button carrying `command`
    /// and its place in the menu.
    fn menu(&self, place: Place, command: &str, text: &str) {
        let buttons: Vec<(String, String)> = self
            .projects(place.chat)
            .iter()
            .enumerate()
            .map(|(index, dir)| (tilde(dir), format!("{command} {index}")))
            .collect();
        if buttons.is_empty() {
            self.say(
                place,
                "no project has run here yet; `/new <directory>` opens one",
            );
            return;
        }
        self.telegram.menu(place, text, &buttons);
    }

    /// The session in `place` heard from last, which is where a message that replies to
    /// nothing goes. A session posting elsewhere stays out of reach, so a project never
    /// answers in a chat it is not posted to, and a topic holds its own conversations.
    fn latest(&self, place: Place) -> Option<String> {
        self.sessions
            .iter()
            .filter(|(_, session)| self.thread(session).place == place)
            .max_by_key(|(_, session)| session.seen)
            .map(|(id, _)| id.clone())
    }

    /// Posts a message to reply to with the first prompt of a new conversation. Nothing
    /// is started yet, so an anchor left alone costs nothing.
    fn anchor(&self, place: Place, argument: &str) -> Option<i64> {
        let Some(cwd) = expand(argument).filter(|cwd| cwd.is_dir()) else {
            self.say(place, &format!("{} is not a directory", code(argument)));
            return None;
        };
        let head = hook::head(&hook::project(&cwd), NEW, None);
        let message = hook::compose(&head, "", "", &hook::prose(&cwd.to_string_lossy()));
        let placeholder = format!("first prompt in {}", tilde(&cwd));
        self.telegram.anchor(place, &message, &placeholder, None)
    }

    /// Opens a window for a conversation, a new one or the session `resume` names, and
    /// keeps its first prompt until the session there reports that it is ready. A
    /// session already being resumed gets no second window, which would run it twice.
    fn open(&mut self, dir: PathBuf, resume: Option<String>, ask: Ask) {
        let resuming =
            resume.is_some() && self.opening.iter().any(|opening| opening.resume == resume);
        if !resuming && let Err(error) = tmux::open(&dir, resume.as_deref()) {
            self.say(ask.place, &format!("tmux: {}", hook::prose(&error)));
            return;
        }
        self.opening.push(Opening { dir, resume, ask });
    }

    /// Types into the session whose id starts with `address`, resuming it first when it
    /// has exited.
    fn send(&mut self, address: &str, ask: Ask) {
        let place = ask.place;
        let Some((id, session)) = self.sessions.iter().find(|(id, _)| id.starts_with(address))
        else {
            match self
                .ended
                .iter()
                .find(|ended| ended.id.starts_with(address))
            {
                Some(ended) => self.open(ended.dir.clone(), Some(ended.id.clone()), ask),
                None => self.say(
                    place,
                    &format!("`{address}` is not a session this resident has seen"),
                ),
            }
            return;
        };
        let id = id.clone();
        let Some(pane) = session.pane.clone() else {
            // Naming the terminal is what tells a session started as a background job,
            // which runs on a pty of its own, from one whose pane went away.
            let short = &id[..8.min(id.len())];
            self.say(
                place,
                &match tmux::controlling_tty(session.pid) {
                    Some(tty) => format!("`{short}` is on `{tty}`, which no tmux pane holds"),
                    None => format!("`{short}` has no terminal to type into"),
                },
            );
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
            self.say(
                place,
                &format!("that terminal no longer holds the session{screen}"),
            );
            return;
        }
        if let Err(error) = pane.deliver(&ask.text) {
            self.say(place, &format!("tmux: {}", hook::prose(&error)));
            return;
        }
        self.sessions
            .get_mut(&id)
            .expect("the session just found")
            .asked
            .push_back(ask);
    }

    /// A status line reports whenever the session redraws it, which is too often for the
    /// state file and cheap to wait for again after a restart.
    fn status(&mut self, status: Status) {
        let now = unix_millis(SystemTime::now()) / 1000;
        if let Some(limits) = status.rate_limits {
            self.limits = Some((limits, now));
        }
        if let (Some(window), Some(session)) = (
            status.context_window,
            self.sessions.get_mut(&status.session_id),
        ) {
            session.window = Some((window, now));
        }
    }

    /// The plan's limits, and how full the context is of the session a message replying
    /// to `replied` would reach. With such a session the answer goes under its head, so a
    /// reply to the answer reaches it too. When a limit resets is written by each reader's
    /// client, in their own zone.
    fn usage(&self, place: Place, asked: i64, replied: &Value) {
        let address = match self.addressee(place, replied) {
            Ok(address) => address.filter(|address| address != NEW),
            Err(error) => return self.say(place, error),
        };
        let reached = address.and_then(|address| {
            self.sessions
                .iter()
                .find(|(id, _)| id.starts_with(&address))
        });
        let now = unix_millis(SystemTime::now()) / 1000;
        let mut rows = Vec::new();
        let mut ages = Vec::new();
        if let Some((_, session)) = reached {
            match &session.window {
                Some((window, at)) => {
                    ages.push(format!("context {}", ago_since(now, *at)));
                    rows.push(match (&window.current_usage, window.used_percentage) {
                        (Some(usage), Some(percentage)) => format!(
                            "{}context {percentage:.0}%, {} of {}",
                            gauge(percentage),
                            tokens(usage.uncached + usage.cache_written + usage.cache_read),
                            tokens(window.context_window_size),
                        ),
                        _ => "context: nothing has been sent yet".to_owned(),
                    });
                }
                None => rows.push("context: not reported yet".to_owned()),
            }
        }
        match &self.limits {
            Some((limits, at)) => {
                for (name, limit) in [("5-hour", &limits.five_hour), ("7-day", &limits.seven_day)] {
                    if let Some(limit) = limit {
                        let left = Duration::from_secs(limit.resets_at.saturating_sub(now));
                        rows.push(format!(
                            "{}{name} {:.0}%, resets in {}\n{UNDER}{}",
                            gauge(limit.used_percentage),
                            limit.used_percentage,
                            until(left, " "),
                            moment(limit.resets_at, &utc(limit.resets_at)),
                        ));
                    }
                }
                ages.push(format!("limits {}", ago_since(now, *at)));
            }
            None => rows.push("limits: not reported yet".to_owned()),
        }
        if !ages.is_empty() {
            rows.push(format!("reported: {}", ages.join(", ")));
        }
        let said = rows.join("\n");
        let answer = match reached {
            Some((id, session)) => format!(
                "<b>{}</b> <code>{}</code>\n{said}",
                hook::html(&hook::name(&session.dir)),
                hook::address(id, None),
            ),
            None => said,
        };
        self.telegram.html(place, &answer, asked);
    }

    fn say(&self, place: Place, text: &str) {
        self.telegram.send(place, text, Sound::Silent, None);
    }
}

/// The chat message that carried a prompt, when klaude is the one that typed it.
/// Anything asked before the match never reached a prompt, so it goes with the match.
fn pair(asked: &mut VecDeque<Ask>, prompt: &str) -> Option<Thread> {
    let at = asked.iter().position(|ask| ask.text == prompt)?;
    asked.drain(..=at).next_back().map(|ask| Thread {
        place: ask.place,
        prompt: Some(ask.message),
    })
}

/// The `/compact` klaude typed into a session and Claude Code has yet to finish.
fn compaction(asked: &VecDeque<Ask>) -> Option<&Ask> {
    asked.iter().find(|ask| {
        ask.text
            .strip_prefix("/compact")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    })
}

/// The address in the head of a message klaude posted, which is the session it belongs
/// to or the anchor of a conversation that has not started.
fn address(message: &Value) -> Option<String> {
    let code = headed(message)
        .or_else(|| coded(&message["caption"], &message["caption_entities"]))
        .or_else(|| coded(&message["text"], &message["entities"]))?;
    let session = code.split('/').next()?;
    (!session.is_empty()).then(|| session.to_owned())
}

fn headed(message: &Value) -> Option<String> {
    let spans = paragraph(message, 0)?.as_array()?;
    let code = spans.iter().find(|span| span["type"] == "code")?;
    Some(plain(&code["text"]))
}

/// The code span of a message klaude posted as text with entities: a file, whose head is
/// its caption, or an HTML message. Entities count UTF-16 code units.
fn coded(text: &Value, entities: &Value) -> Option<String> {
    let text: Vec<u16> = text.as_str()?.encode_utf16().collect();
    let code = entities
        .as_array()?
        .iter()
        .find(|entity| entity["type"] == "code")?;
    let start = usize::try_from(code["offset"].as_u64()?).ok()?;
    let end = start + usize::try_from(code["length"].as_u64()?).ok()?;
    String::from_utf16(text.get(start..end)?).ok()
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

/// The start of a prompt's first line, as a button has room for.
fn glimpse(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= PROMPT_MAX {
        return line.to_owned();
    }
    line.chars()
        .take(PROMPT_MAX)
        .chain("\u{2026}".chars())
        .collect()
}

/// How long ago, in the largest unit it fills.
fn ago(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

/// A percentage as a bar ten cells wide, filled to the eighth of a cell, and the gap to
/// the words after it. The monospace font is what lines the blocks up from row to row.
fn gauge(percentage: f64) -> String {
    const CELLS: usize = 10;
    const PARTS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    // Clamped to the bar's eighty eighths first, so the cast neither truncates nor wraps.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let eighths = (percentage.clamp(0.0, 100.0) * 0.8).round() as usize;
    let full = eighths / 8;
    let part = PARTS.get((eighths % 8).wrapping_sub(1));
    let empty = CELLS - full - usize::from(part.is_some());
    format!(
        "<code>{}{}{}</code>  ",
        "█".repeat(full),
        part.map(char::to_string).unwrap_or_default(),
        "░".repeat(empty)
    )
}

/// What lines a row up under the words after a gauge. The gauge is in the monospace
/// font and the row in the reader's own, where twelve en spaces of half an em come
/// closest to the gauge's ten cells of about 0.6 em.
const UNDER: &str = "\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}  ";

/// A moment each reader's client writes in their own zone as weekday, date and time,
/// with `fallback` for a client that cannot.
fn moment(unix: u64, fallback: &str) -> String {
    format!("<tg-time unix=\"{unix}\" format=\"wDt\">{fallback}</tg-time>")
}

fn utc(unix: u64) -> String {
    format!("{:02}:{:02} UTC", unix % 86400 / 3600, unix % 3600 / 60)
}

fn ago_since(now: u64, then: u64) -> String {
    ago(Duration::from_secs(now.saturating_sub(then)))
}

/// How long until a limit resets, to the minute, with `gap` between the two units.
fn until(left: Duration, gap: &str) -> String {
    let minutes = left.as_secs().div_ceil(60);
    match minutes {
        0..60 => format!("{minutes}m"),
        60..1440 => format!("{}h{gap}{}m", minutes / 60, minutes % 60),
        _ => format!("{}d{gap}{}h", minutes / 1440, minutes % 1440 / 60),
    }
}

/// The figures `/usage` answers with, as one line of code under the answer that closes
/// a turn: how full the context is, then how much of each limit is used and how long
/// until it resets.
fn status_line(window: Option<&(Window, u64)>, limits: Option<&(Limits, u64)>) -> Option<String> {
    let now = unix_millis(SystemTime::now()) / 1000;
    let context = window.and_then(|(window, _)| {
        let usage = window.current_usage.as_ref()?;
        Some(format!(
            "{:.0}% {}/{}",
            window.used_percentage?,
            tokens(usage.uncached + usage.cache_written + usage.cache_read),
            tokens(window.context_window_size),
        ))
    });
    let limits = limits
        .into_iter()
        .flat_map(|(limits, _)| [&limits.five_hour, &limits.seven_day])
        .flatten()
        .map(|limit| {
            let left = Duration::from_secs(limit.resets_at.saturating_sub(now));
            format!("{:.0}% {}", limit.used_percentage, until(left, ""))
        });
    let figures: Vec<String> = context.into_iter().chain(limits).collect();
    (!figures.is_empty()).then(|| format!("`{}`", figures.join(" · ")))
}

/// A token count the way Claude Code writes one, as `45.6k` or `1m`.
fn tokens(count: u64) -> String {
    let (tenths, unit) = match count {
        0..1000 => return count.to_string(),
        1000..1_000_000 => ((count + 50) / 100, "k"),
        _ => ((count + 50_000) / 100_000, "m"),
    };
    match tenths % 10 {
        0 => format!("{}{unit}", tenths / 10),
        tenth => format!("{}.{tenth}{unit}", tenths / 10),
    }
}

/// A command klaude answers and its argument. A group's command menu names the bot a
/// command is for, as `/new@bot`.
fn command(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (word, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let name = word.split_once('@').map_or(word, |(name, _)| name);
    Some((name, argument.trim()))
}

/// `text` without the `@<bot>` a group's command menu appends to a command, which Claude
/// Code would take for part of the command's name.
fn unaddressed(text: &str) -> String {
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    match text[..end].find('@') {
        Some(at) if text.starts_with('/') => format!("{}{}", &text[..at], &text[end..]),
        _ => text.to_owned(),
    }
}

/// The label of the button in a menu that carries `data`.
fn label(menu: &Value, data: &str) -> Option<String> {
    menu["reply_markup"]["inline_keyboard"]
        .as_array()?
        .iter()
        .flat_map(|row| row.as_array().into_iter().flatten())
        .find(|button| button["callback_data"] == data)?["text"]
        .as_str()
        .map(str::to_owned)
}

/// A directory as it would be typed in the chat, which `expand` reads back.
fn tilde(dir: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|home| dir.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
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

/// Where a turn starts in the list of status words, so two turns running at once do not
/// step through it together.
fn seed(prompt_id: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    prompt_id.hash(&mut hasher);
    hasher.finish() % WORDS.len() as u64
}

/// A group's id is negative.
fn rewrite(chat: i64) -> Duration {
    if chat < 0 { GROUP_REWRITE } else { REWRITE }
}

/// The body of the message showing an open segment: what it has said, then the status
/// line, which is what a turn spending minutes in tool calls reads by.
fn running(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_owned()
    } else {
        format!("{text}{BREAK}{status}")
    }
}

/// The word changes once per refresh, so a turn sitting in a long tool call keeps
/// showing a line that differs from the last one.
fn status(elapsed: Duration, seed: u64) -> String {
    let step = seed + elapsed.as_secs() / REFRESH.as_secs();
    let word = WORDS[usize::try_from(step).expect("a turn's seconds") % WORDS.len()];
    format!("✻ {word}… ({})", took(elapsed).trim())
}

/// How long a tool call took, in the units a tool call runs in.
fn spent(elapsed: Duration) -> String {
    match elapsed.as_millis() {
        millis @ ..1000 => format!("{millis}ms"),
        _ => took(elapsed).trim().to_owned(),
    }
}

/// What a failed tool reported, in one line.
fn why(error: &str) -> String {
    let first = error.lines().next().unwrap_or_default().trim();
    if first.chars().count() <= WHY_MAX {
        return first.to_owned();
    }
    first
        .chars()
        .take(WHY_MAX)
        .chain("\u{2026}".chars())
        .collect()
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
        assert_eq!(took(Duration::from_mins(131)), " 2h11m");
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
    fn a_group_is_rewritten_less_often_than_a_private_chat() {
        assert_eq!(rewrite(-1001), GROUP_REWRITE);
        assert_eq!(rewrite(7), REWRITE);
    }

    #[test]
    fn a_seed_is_stable_per_turn_and_lands_on_a_word() {
        assert_eq!(seed("turn_1"), seed("turn_1"));
        assert_ne!(seed("turn_1"), seed("turn_2"));
        assert!(seed("") < WORDS.len() as u64);
    }

    #[test]
    fn deltas_arriving_out_of_order_still_read_in_order() {
        let mut segment = Segment::new("msg_1", Body::Text(BTreeMap::new()));
        let Body::Text(chunks) = &mut segment.body else {
            panic!("a text segment")
        };
        chunks.insert(2, "third".to_owned());
        chunks.insert(0, "first ".to_owned());
        chunks.insert(1, "second ".to_owned());
        assert_eq!(segment.text(), "first second third");
    }

    fn call(name: &str, subject: &str, outcome: Outcome) -> Call {
        Call {
            id: name.to_owned(),
            agent: None,
            name: name.to_owned(),
            description: String::new(),
            subject: subject.to_owned(),
            outcome,
        }
    }

    #[test]
    fn a_call_reads_as_its_tool_its_subject_and_how_it_went() {
        assert_eq!(
            call("Read", "src/listen.rs", Outcome::Running).line(),
            "○ **Read**  `src/listen.rs`"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_millis(1400))
            )
            .line(),
            "● **Bash**  `cargo test` **1s**"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_millis(12))
            )
            .line(),
            "● **Bash**  `cargo test` **12ms**"
        );
        // What a failure reported reads on a line of its own.
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Failed(Duration::from_secs(4), "Exit code 1".to_owned())
            )
            .line(),
            "× **Bash**  `cargo test` **4s**  \n⎿ Exit code 1"
        );
        // A tool that describes its calls says that first and shows the command under it.
        let described = Call {
            description: "run the tests".to_owned(),
            ..call(
                "Bash",
                "cargo test",
                Outcome::Failed(Duration::from_secs(4), "Exit code 1".to_owned()),
            )
        };
        assert_eq!(
            described.line(),
            "× **Bash**  run the tests **4s**  \n⎿ `cargo test`  \n⎿ Exit code 1"
        );
        let markup = Call {
            description: "find *.rs in _src_".to_owned(),
            ..call("Grep", "fn seal", Outcome::Running)
        };
        assert_eq!(
            markup.line(),
            "○ **Grep**  find \\*\\.rs in \\_src\\_  \n⎿ `fn seal`"
        );
        let subagent = Call {
            agent: Some("Explore".to_owned()),
            ..call("Grep", "fn seal", Outcome::Running)
        };
        assert_eq!(subagent.line(), "○ [Explore] **Grep**  `fn seal`");
    }

    #[test]
    fn a_command_travels_in_a_span_past_any_backticks_it_carries() {
        assert_eq!(code("cargo test"), "`cargo test`");
        assert_eq!(code("echo ```x```"), "```` echo ```x``` ````");
    }

    #[test]
    fn a_run_keeps_its_calls_on_lines_of_their_own() {
        let listed = listing(&[
            call("Read", "src/listen.rs", Outcome::Running),
            call("Bash", "cargo test", Outcome::Done(Duration::from_secs(1))),
        ]);
        assert_eq!(
            listed,
            "○ **Read**  `src/listen.rs`  \n● **Bash**  `cargo test` **1s**"
        );
    }

    #[test]
    fn a_long_run_counts_the_calls_it_stops_listing() {
        let calls: Vec<Call> = (0..RUN_MAX + 3)
            .map(|index| call("Read", &format!("file{index}"), Outcome::Running))
            .collect();
        let listed = listing(&calls);
        assert!(listed.contains("… 3 earlier"), "the run reads {listed}");
        assert!(
            !listed.contains("**Read**  `file2`"),
            "the third call is still listed"
        );
        assert!(
            listed.contains("**Read**  `file3`"),
            "the fourth call is dropped"
        );
        assert!(
            listed.contains(&format!("`file{}`", RUN_MAX + 2)),
            "the last call is listed"
        );
        // The calls it lists, and the line that counts the ones it does not.
        assert_eq!(listed.lines().count(), RUN_MAX + 1);
    }

    #[test]
    fn a_failure_reports_its_first_line_alone() {
        assert_eq!(why("Exit code 1\nError: nope"), "Exit code 1");
        assert_eq!(why(&"x".repeat(WHY_MAX + 5)).chars().count(), WHY_MAX + 1);
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
        let document = serde_json::json!({
            "caption": "🐱 01234567/fedcba98 5s",
            "caption_entities": [
                {"type": "bold", "offset": 0, "length": 2},
                {"type": "code", "offset": 3, "length": 17},
            ],
        });
        assert_eq!(address(&document).as_deref(), Some("01234567"));
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

    const HERE: Place = Place {
        chat: 7,
        topic: None,
    };

    #[test]
    fn a_prompt_klaude_typed_is_paired_with_the_message_that_asked_for_it() {
        let ask = |text: &str, message| Ask {
            text: text.to_owned(),
            place: HERE,
            message,
        };
        let mut asked = VecDeque::from([ask("first", 1), ask("second", 2)]);
        assert_eq!(
            pair(&mut asked, "second"),
            Some(Thread {
                place: HERE,
                prompt: Some(2)
            })
        );
        assert!(asked.is_empty());

        let mut asked = VecDeque::from([ask("from the phone", 3)]);
        assert_eq!(pair(&mut asked, "typed in the terminal"), None);
        assert_eq!(asked.len(), 1);
    }

    #[test]
    fn a_compact_klaude_typed_is_found_with_or_without_instructions() {
        let found = |texts: &[&str]| {
            let asked = VecDeque::from_iter(texts.iter().zip(1..).map(|(text, message)| Ask {
                text: (*text).to_owned(),
                place: HERE,
                message,
            }));
            compaction(&asked).map(|ask| ask.message)
        };
        assert_eq!(found(&["/compactor", "/compact"]), Some(2));
        assert_eq!(found(&["/compact keep the plan"]), Some(1));
        assert_eq!(found(&["compact it"]), None);
    }

    #[test]
    fn an_age_reads_in_the_largest_unit_it_fills() {
        assert_eq!(ago(Duration::from_secs(59)), "59s ago");
        assert_eq!(ago(Duration::from_secs(3599)), "59m ago");
        assert_eq!(ago(Duration::from_secs(86399)), "23h ago");
        assert_eq!(ago(Duration::from_hours(72)), "3d ago");
    }

    #[test]
    fn a_command_is_read_with_or_without_the_bot_it_names() {
        assert_eq!(command("/new ~/p"), Some(("new", "~/p")));
        assert_eq!(command("/new@klaude_bot  ~/p "), Some(("new", "~/p")));
        assert_eq!(command("/new@klaude_bot"), Some(("new", "")));
        assert_eq!(
            command("/compact keep it short"),
            Some(("compact", "keep it short"))
        );
        assert_eq!(command("new"), None);
    }

    #[test]
    fn a_gauge_fills_to_the_eighth_of_a_cell() {
        let bar = |percentage| {
            gauge(percentage)
                .replace("<code>", "")
                .replace("</code>  ", "")
        };
        assert_eq!(bar(0.0), "░░░░░░░░░░");
        assert_eq!(bar(1.0), "▏░░░░░░░░░");
        assert_eq!(bar(56.0), "█████▋░░░░");
        assert_eq!(bar(100.0), "██████████");
        assert_eq!(bar(120.0), "██████████");
    }

    #[test]
    fn a_reset_reads_to_the_minute() {
        assert_eq!(until(Duration::from_secs(59), " "), "1m");
        assert_eq!(until(Duration::from_mins(209), " "), "3h 29m");
        assert_eq!(until(Duration::from_hours(62), " "), "2d 14h");
    }

    #[test]
    fn a_token_count_reads_as_claude_code_writes_it() {
        assert_eq!(tokens(75), "75");
        assert_eq!(tokens(45_556), "45.6k");
        assert_eq!(tokens(1_000_000), "1m");
    }

    #[test]
    fn a_command_is_typed_without_the_bot_it_names() {
        assert_eq!(unaddressed("/compact@klaude_bot"), "/compact");
        assert_eq!(
            unaddressed("/compact@klaude_bot keep a@b"),
            "/compact keep a@b"
        );
        assert_eq!(unaddressed("/compact keep a@b"), "/compact keep a@b");
        assert_eq!(unaddressed("mail a@b"), "mail a@b");
    }

    #[test]
    fn a_directory_under_home_is_written_from_a_tilde_and_read_back() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("a home"));
        assert_eq!(tilde(&home), "~");
        assert_eq!(tilde(&home.join("dev/p")), "~/dev/p");
        assert_eq!(tilde(Path::new("/srv/p")), "/srv/p");
    }

    #[test]
    fn a_pressed_button_is_found_by_the_data_it_carries() {
        let menu = serde_json::json!({"reply_markup": {"inline_keyboard": [
            [{"text": "~/a", "callback_data": "new 0"}],
            [{"text": "~/b", "callback_data": "new 1"}],
        ]}});
        assert_eq!(label(&menu, "new 1").as_deref(), Some("~/b"));
        assert_eq!(label(&menu, "new 2"), None);
    }

    #[test]
    fn a_directory_is_expanded_from_a_leading_tilde() {
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(expand("~").as_deref(), Some(Path::new(&home)));
        assert_eq!(expand(""), None);
        assert_eq!(expand("/definitely/not/here"), None);
    }
}

//! The one process per machine that owns every Telegram call of every session, and the
//! only reader of the chat. It holds the calls in order per turn, and it is where a
//! message from the chat becomes keystrokes in a session's terminal.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use crate::hook::{self, Event};
use crate::telegram::{Sound, Telegram};
use crate::tmux::{self, Pane};

/// The longest the message showing an open segment goes without its clock advancing,
/// so a turn that goes quiet inside a long tool call still reads as running.
const REFRESH: Duration = Duration::from_secs(30);
/// The shortest gap between two rewrites of that message. It is also how long a turn
/// runs before the message exists, so a turn answered at once leaves nothing to take
/// back.
const REWRITE: Duration = Duration::from_secs(3);
/// How long a tool call waits before it is filed. An assistant message's last flush
/// reaches the resident tens of milliseconds after the hook of the tool call that
/// message ends with, so a call filed as it is announced stands above the words that
/// introduce it.
const SETTLE: Duration = Duration::from_millis(100);
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

/// What reaches the socket, from a hook, from the poller reading the chat, or from
/// `klaude send`, which waits at `reply` for an empty answer or what went wrong, and names
/// no session when it ran outside Claude Code.
#[derive(Deserialize)]
#[serde(untagged)]
enum Arrival {
    Hook(Box<Handoff>),
    Chat {
        message: Value,
    },
    Upload {
        session: Option<String>,
        file: PathBuf,
        reply: PathBuf,
    },
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
    let answers = socket.try_clone().expect("socket");
    std::thread::spawn(|| poll(&socket_path()));
    let arrivals = read(socket);

    let mut machine = Machine {
        telegram: Telegram::new(),
        answers,
        sessions: BTreeMap::new(),
        opening: Vec::new(),
    };
    loop {
        match arrivals.recv_timeout(POLL) {
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
}

impl Segment {
    fn new(id: &str, body: Body) -> Self {
        Self {
            id: id.to_owned(),
            body,
            written: None,
            posted: None,
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

/// The chat a turn is posted in and the message there carrying what was asked, which the
/// rest of the turn replies to. A turn asked from the phone stays in the chat it was
/// asked in, and any other goes to `CHAT_ID`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Thread {
    chat: i64,
    prompt: Option<i64>,
}

/// Text from the chat and the message that carried it, which is the thread the turn it
/// becomes replies into.
struct Ask {
    text: String,
    chat: i64,
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
}

impl Session {
    fn head(&self, id: &str, prompt: Option<&str>) -> String {
        hook::head(&hook::project(&self.dir), id, prompt)
    }
}

struct Machine {
    telegram: Telegram,
    /// Where `klaude send` hears how its upload went.
    answers: UnixDatagram,
    sessions: BTreeMap<String, Session>,
    /// What the chat asked, waiting for the session whose window it opened, by directory.
    opening: Vec<(PathBuf, Ask)>,
}

impl Machine {
    fn arrival(&mut self, arrival: Arrival) {
        match arrival {
            Arrival::Hook(handoff) => {
                self.hook(handoff.pid, handoff.tmux.zip(handoff.pane), &handoff.event);
            }
            Arrival::Chat { message } => self.chat(&message),
            Arrival::Upload {
                session,
                file,
                reply,
            } => {
                let answer = self
                    .upload(session.as_deref(), &file)
                    .err()
                    .unwrap_or_default();
                if let Err(error) = self.answers.send_to(answer.as_bytes(), &reply) {
                    eprintln!("answer {}: {error}", reply.display());
                }
            }
        }
    }

    fn hook(&mut self, pid: u32, tmux: Option<(String, String)>, event: &Event) {
        let id = event.session_id.clone();
        let pane = tmux.map(|(server, pane)| Pane::new(&server, &pane));
        let directory = event.directory();
        let session = self.sessions.entry(id.clone()).or_insert_with(|| Session {
            dir: PathBuf::from(&event.cwd),
            pid,
            pane: pane.clone(),
            queued: VecDeque::new(),
            asked: VecDeque::new(),
            turn: None,
            done: None,
            seen: Instant::now(),
        });
        session.pid = pid;
        session.pane = pane;
        session.seen = Instant::now();
        if let Some(directory) = directory {
            session.dir = directory;
        }

        match event.hook_event_name.as_str() {
            "SessionStart" => self.started(&id),
            "UserPromptSubmit" => self.submitted(&id, event),
            "MessageDisplay" => self.delta(&id, event),
            "PreToolUse" => self.calling(&id, event),
            "PostToolUse" | "PostToolUseFailure" => self.called(&id, event),
            "Stop" | "StopFailure" => self.finish(&id, event),
            // A session waiting on a dialog is the other thing worth coming back to.
            "Notification" => self.aside(&id, event, Sound::Ring),
            _ => self.aside(&id, event, Sound::Silent),
        }
    }

    /// A session is ready for input once it says so, which is after the dialog that
    /// asks whether its folder is trusted. A conversation opened from the chat is
    /// waiting for exactly this to type its first prompt.
    fn started(&mut self, id: &str) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let Some(index) = self.opening.iter().position(|(cwd, _)| *cwd == session.dir) else {
            return;
        };
        let (_, ask) = self.opening.remove(index);
        self.send(id, ask);
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
                self.telegram.acknowledge(thread.chat, prompt);
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
            let chat = self.telegram.chat();
            // The phone's owner asked this, so it arrives without a sound.
            Thread {
                chat,
                prompt: self.telegram.send(chat, &message, Sound::Silent, None),
            }
        };
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
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
        let chat = self.telegram.chat();
        let Some(session) = self.sessions.get_mut(id) else {
            return false;
        };
        session.turn = Some(Turn {
            prompt_id: named.to_owned(),
            seed: seed(named),
            started: Instant::now(),
            thread: session
                .queued
                .pop_front()
                .unwrap_or(Thread { chat, prompt: None }),
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
        let Some(Body::Text(chunks)) = self
            .sessions
            .get_mut(id)
            .and_then(|s| s.turn.as_mut())
            .and_then(|t| t.segment.as_mut())
            .map(|segment| &mut segment.body)
        else {
            return;
        };
        chunks.insert(index, delta.clone());
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
                self.telegram.edit(thread.chat, message, &done);
                Some(message)
            }
            // A segment that ran its course inside one rewrite has no message yet.
            None => self
                .telegram
                .send(thread.chat, &done, Sound::Silent, thread.prompt),
        };
        segment.posted = message.map(|message| (message, elapsed));
        let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
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
            turn.thread.chat,
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
        let message = hook::message(event, &head, &took(turn.started.elapsed()));
        // The one sound of the turn: the reply is complete and worth coming back to.
        let thread = turn.thread;
        self.telegram
            .send(thread.chat, &message, Sound::Ring, thread.prompt);
        // This event carries the last segment's text, so the message that was showing
        // it goes rather than standing above the one that repeats it.
        if let Some(live) = turn.live {
            self.telegram.delete(thread.chat, live);
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
        self.telegram
            .send(thread.chat, &message, sound, thread.prompt);
    }

    /// A file the session asked to show, posted in the thread of its turn below
    /// everything the turn has said so far. Its caption is the head a message of the
    /// turn carries, so a reply to the file reaches the session. A file sent from
    /// outside any session goes to `CHAT_ID` bare.
    fn upload(&mut self, id: Option<&str>, file: &Path) -> Result<(), String> {
        let Some(id) = id else {
            return self
                .telegram
                .document(self.telegram.chat(), file, None, None);
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
            html(&hook::name(&session.dir))
        );
        let thread = self.thread(session);
        self.telegram
            .document(thread.chat, file, Some(&caption), thread.prompt)
    }

    /// Where a session's messages go: its turn's thread, or `CHAT_ID` between turns.
    fn thread(&self, session: &Session) -> Thread {
        session.turn.as_ref().map_or(
            Thread {
                chat: self.telegram.chat(),
                prompt: None,
            },
            |turn| turn.thread,
        )
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
            let live = self
                .sessions
                .get_mut(&id)
                .and_then(|session| session.turn.as_mut())
                .and_then(|turn| Some((turn.thread.chat, turn.live.take()?)));
            if let Some((chat, message)) = live {
                self.telegram.delete(chat, message);
            }
            self.sessions.remove(&id);
        }

        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            self.place(&id);
            self.show(&id);
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
        let elapsed = turn.started.elapsed();
        if elapsed < REWRITE {
            return;
        }
        let Some(segment) = turn.segment.as_ref() else {
            return;
        };
        let text = segment.text();
        let due = match &segment.written {
            None => true,
            Some((written, at)) if *written == text => at.elapsed() >= REFRESH,
            Some((_, at)) => at.elapsed() >= REWRITE,
        };
        if !due {
            return;
        }
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
                self.telegram.edit(thread.chat, message, &shown);
                Some(message)
            }
            None => self
                .telegram
                .send(thread.chat, &shown, Sound::Silent, thread.prompt),
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
    /// the session heard from last.
    fn chat(&mut self, message: &Value) {
        if !self.telegram.accepts(message) {
            // The ids to put in the env file are read from here.
            eprintln!(
                "ignored: chat {} {:?} from {}",
                message["chat"]["id"],
                message["chat"]["title"].as_str().unwrap_or_default(),
                message["from"]["id"]
            );
            return;
        }
        let Some(chat) = message["chat"]["id"].as_i64() else {
            return;
        };
        let text = message["text"].as_str().unwrap_or_default().trim();
        if text.is_empty() {
            return;
        }
        if let Some(argument) = text.strip_prefix("/new") {
            // A group's command menu names the bot a command is for, as `/new@bot`.
            let argument = match argument.strip_prefix('@') {
                Some(named) => named
                    .split_once(char::is_whitespace)
                    .map_or("", |(_, rest)| rest),
                None => argument,
            };
            self.anchor(chat, argument.trim());
            return;
        }
        let Some(carrier) = message["message_id"].as_i64() else {
            return;
        };
        let ask = Ask {
            text: text.to_owned(),
            chat,
            message: carrier,
        };
        let replied = &message["reply_to_message"];
        match address(replied) {
            Some(address) if address == NEW => match body(replied) {
                Some(cwd) => self.open(Path::new(&cwd), ask),
                None => self.say(chat, "that anchor names no directory"),
            },
            Some(address) => self.send(&address, ask),
            None => match self.latest() {
                Some(address) => self.send(&address, ask),
                None => self.say(
                    chat,
                    "no session is running here; `/new <directory>` opens one",
                ),
            },
        }
    }

    /// The session heard from last, which is where a message that replies to nothing
    /// goes.
    fn latest(&self) -> Option<String> {
        self.sessions
            .iter()
            .max_by_key(|(_, session)| session.seen)
            .map(|(id, _)| id.clone())
    }

    /// A message to reply to with the first prompt of a new conversation. Nothing is
    /// started yet, so an anchor left alone costs nothing.
    fn anchor(&mut self, chat: i64, argument: &str) {
        let Some(cwd) = expand(argument) else {
            self.say(chat, "`/new <directory>`");
            return;
        };
        if !cwd.is_dir() {
            self.say(chat, &format!("`{}` is not a directory", cwd.display()));
            return;
        }
        let head = hook::head(&hook::project(&cwd), NEW, None);
        let message = hook::compose(&head, "", "", &hook::prose(&cwd.to_string_lossy()));
        self.telegram.send(chat, &message, Sound::Silent, None);
    }

    /// Opens a window for a conversation and keeps its first prompt until the session
    /// there reports that it is ready.
    fn open(&mut self, cwd: &Path, ask: Ask) {
        if let Err(error) = tmux::open(cwd) {
            self.say(ask.chat, &format!("tmux: {}", hook::prose(&error)));
            return;
        }
        self.opening.push((cwd.to_owned(), ask));
    }

    /// Types into the session whose id starts with `address`.
    fn send(&mut self, address: &str, ask: Ask) {
        let chat = ask.chat;
        let Some((id, session)) = self.sessions.iter().find(|(id, _)| id.starts_with(address))
        else {
            self.say(chat, &format!("`{address}` is not a session running here"));
            return;
        };
        let id = id.clone();
        let Some(pane) = session.pane.clone() else {
            // Naming the terminal is what tells a session started as a background job,
            // which runs on a pty of its own, from one whose pane went away.
            let short = &id[..8.min(id.len())];
            self.say(
                chat,
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
                chat,
                &format!("that terminal no longer holds the session{screen}"),
            );
            return;
        }
        if let Err(error) = pane.deliver(&ask.text) {
            self.say(chat, &format!("tmux: {}", hook::prose(&error)));
            return;
        }
        self.sessions
            .get_mut(&id)
            .expect("the session just found")
            .asked
            .push_back(ask);
    }

    fn say(&self, chat: i64, text: &str) {
        self.telegram.send(chat, text, Sound::Silent, None);
    }
}

/// The chat message that carried a prompt, when klaude is the one that typed it.
/// Anything asked before the match never reached a prompt, so it goes with the match.
fn pair(asked: &mut VecDeque<Ask>, prompt: &str) -> Option<Thread> {
    let at = asked.iter().position(|ask| ask.text == prompt)?;
    asked.drain(..=at).next_back().map(|ask| Thread {
        chat: ask.chat,
        prompt: Some(ask.message),
    })
}

/// The address in the head of a message klaude posted, which is the session it belongs
/// to or the anchor of a conversation that has not started.
fn address(message: &Value) -> Option<String> {
    let code = headed(message).or_else(|| captioned(message))?;
    let session = code.split('/').next()?;
    (!session.is_empty()).then(|| session.to_owned())
}

fn headed(message: &Value) -> Option<String> {
    let spans = paragraph(message, 0)?.as_array()?;
    let code = spans.iter().find(|span| span["type"] == "code")?;
    Some(plain(&code["text"]))
}

/// A file klaude posted carries its head as a caption, whose entities count UTF-16 code
/// units.
fn captioned(message: &Value) -> Option<String> {
    let caption: Vec<u16> = message["caption"].as_str()?.encode_utf16().collect();
    let code = message["caption_entities"]
        .as_array()?
        .iter()
        .find(|entity| entity["type"] == "code")?;
    let start = usize::try_from(code["offset"].as_u64()?).ok()?;
    let end = start + usize::try_from(code["length"].as_u64()?).ok()?;
    String::from_utf16(caption.get(start..end)?).ok()
}

/// The characters Telegram's HTML captions reserve.
fn html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

/// Where a turn starts in the list of status words, so two turns running at once do not
/// step through it together.
fn seed(prompt_id: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    prompt_id.hash(&mut hasher);
    hasher.finish() % WORDS.len() as u64
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

    #[test]
    fn a_prompt_klaude_typed_is_paired_with_the_message_that_asked_for_it() {
        let ask = |text: &str, message| Ask {
            text: text.to_owned(),
            chat: 7,
            message,
        };
        let mut asked = VecDeque::from([ask("first", 1), ask("second", 2)]);
        assert_eq!(
            pair(&mut asked, "second"),
            Some(Thread {
                chat: 7,
                prompt: Some(2)
            })
        );
        assert!(asked.is_empty());

        let mut asked = VecDeque::from([ask("from the phone", 3)]);
        assert_eq!(pair(&mut asked, "typed in the terminal"), None);
        assert_eq!(asked.len(), 1);
    }

    #[test]
    fn a_directory_is_expanded_from_a_leading_tilde() {
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(expand("~").as_deref(), Some(Path::new(&home)));
        assert_eq!(expand(""), None);
        assert_eq!(expand("/definitely/not/here"), None);
    }
}

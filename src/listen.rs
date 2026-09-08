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
    std::thread::spawn(|| poll(&socket_path()));
    let arrivals = read(socket);

    let mut machine = Machine {
        telegram: Telegram::from_env(),
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
    subject: String,
    outcome: Outcome,
}

impl Call {
    /// A mark for how it went, the tool and what it is doing, and the time it took. What
    /// a failure reported goes on a line under that, where the eye finds it.
    fn line(&self) -> String {
        let (mark, took, why) = match &self.outcome {
            Outcome::Running => (RUNNING, String::new(), None),
            Outcome::Done(took) => (DONE, format!(" {}", spent(*took)), None),
            Outcome::Failed(took, why) => (FAILED, format!(" {}", spent(*took)), Some(why)),
        };
        let agent = match &self.agent {
            Some(agent) => format!("[{agent}] "),
            None => String::new(),
        };
        let subject = match self.subject.as_str() {
            "" => String::new(),
            subject => format!(" {}", code(subject)),
        };
        let line = format!("{mark} {agent}{}{subject}{took}", self.name);
        match why {
            Some(why) => format!("{line}{BREAK}⎿ {why}"),
            None => line,
        }
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

/// One stretch of a turn, framed into the turn's draft until the next one starts, and
/// posted as a message then. A turn opens with an empty text segment, whose frames are
/// the status line while nothing has been said yet.
struct Segment {
    id: String,
    body: Body,
    /// What the last frame showed and when it went out, absent until the first frame.
    framed: Option<(String, Instant)>,
    /// The message this segment became and the elapsed time stamped on it, which is
    /// what a flush or an outcome arriving later rewrites.
    posted: Option<(i64, Duration)>,
}

impl Segment {
    fn new(id: &str, body: Body) -> Self {
        Self {
            id: id.to_owned(),
            body,
            framed: None,
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

/// Text from the chat and the message that carried it, which is the thread the turn it
/// becomes replies into.
struct Ask {
    text: String,
    message: i64,
}

/// One turn, from the event that started it to its Stop.
struct Turn {
    prompt_id: String,
    /// Every frame of every segment carries this, so one draft animates through the
    /// whole turn rather than one bubble being left behind per segment.
    draft_id: i64,
    started: Instant,
    /// The message carrying what was asked, which the rest of the turn replies to.
    reply_to: Option<i64>,
    segment: Option<Segment>,
    /// The segments the chat already has, kept because what belongs in one goes on
    /// arriving after it was posted.
    sealed: Vec<Segment>,
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
    queued: VecDeque<Option<i64>>,
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
        let posted = if let Some(message) = typed {
            self.telegram.acknowledge(message);
            Some(message)
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
            // The phone's owner asked this, so it arrives without a sound.
            self.telegram.send(&message, Sound::Silent, None)
        };
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        if running {
            session.queued.push_back(posted);
        } else {
            let prompt_id = event.prompt_id.clone().unwrap_or_default();
            session.turn = Some(Turn {
                segment: Some(Segment::new(&prompt_id, Body::Text(BTreeMap::new()))),
                draft_id: draft_id(&prompt_id),
                prompt_id,
                started: Instant::now(),
                reply_to: posted,
                sealed: Vec::new(),
            });
        }
    }

    /// Opens the turn an event belongs to when the event names one that has not been
    /// seen, which is how a queued prompt's turn begins. False for an event of a turn
    /// that already finished: the three hook processes run at once, so a delta can land
    /// after its own `Stop`, and opening a second turn for it would leave a draft
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
        session.turn = Some(Turn {
            prompt_id: named.to_owned(),
            draft_id: draft_id(named),
            started: Instant::now(),
            reply_to: session.queued.pop_front().flatten(),
            segment: Some(Segment::new(named, Body::Text(BTreeMap::new()))),
            sealed: Vec::new(),
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

    /// A tool call joins the run that is open, opening one when the turn was saying
    /// something instead. A run reads as one message, so a turn that talked, worked and
    /// talked again leaves those three in the chat in order.
    fn calling(&mut self, id: &str, event: &Event) {
        let (Some(tool_use_id), Some(name)) = (&event.tool_use_id, &event.tool_name) else {
            return;
        };
        if !self.turn(id, event.prompt_id.as_deref()) {
            return;
        }
        let Some(turn) = self.sessions.get(id).and_then(|s| s.turn.as_ref()) else {
            return;
        };
        if !turn.running_tools() {
            self.seal(id);
            let Some(turn) = self.sessions.get_mut(id).and_then(|s| s.turn.as_mut()) else {
                return;
            };
            turn.segment = Some(Segment::new(tool_use_id, Body::Tools(Vec::new())));
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
        calls.push(Call {
            id: tool_use_id.clone(),
            agent: event.agent_type.clone(),
            name: name.clone(),
            subject: event.subject(),
            outcome: Outcome::Running,
        });
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

    /// A segment that has stopped receiving text is complete, so what its draft was
    /// showing becomes a message. The turn interrupts once, at its end, so this is quiet.
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
        if !text.is_empty() {
            let elapsed = turn.started.elapsed();
            let prompt_id = turn.prompt_id.clone();
            let reply_to = turn.reply_to;
            let draft = turn.draft_id;
            let head = session.head(id, Some(&prompt_id));
            // The tag marks a finished turn, and this segment is the middle of one.
            let posted = hook::compose(&head, &took(elapsed), "", &text);
            segment.posted = self
                .telegram
                .send(&posted, Sound::Silent, reply_to)
                .map(|message| (message, elapsed));
            let status = status(elapsed, draft);
            self.telegram.draft(draft, &frame(&head, "", Some(&status)));
        }
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
        self.telegram
            .edit(message, &hook::compose(&head, &took(elapsed), "", &text));
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
        // The last segment's text is what this event carries, so its draft is dropped
        // rather than posted a second time just above the message that repeats it.
        let head = session.head(id, Some(&turn.prompt_id));
        let message = hook::message(event, &head, &took(turn.started.elapsed()));
        // The one sound of the turn: the reply is complete and worth coming back to.
        self.telegram.send(&message, Sound::Ring, turn.reply_to);
        // Nothing is running any more, so the draft waits out its half minute on the
        // head alone.
        self.telegram.draft(turn.draft_id, &frame(&head, "", None));
    }

    /// Anything else a session reports lands in the thread of the turn it happened in.
    fn aside(&mut self, id: &str, event: &Event, sound: Sound) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let prompt = session.turn.as_ref().map(|turn| turn.prompt_id.clone());
        let head = session.head(id, prompt.as_deref());
        let reply_to = session.turn.as_ref().and_then(|turn| turn.reply_to);
        let message = hook::message(event, &head, "");
        self.telegram.send(&message, sound, reply_to);
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
            if segment
                .framed
                .as_ref()
                .is_some_and(|(framed, at)| *framed == text && at.elapsed() < REFRESH)
            {
                continue;
            }
            let head = session.head(&id, Some(&turn.prompt_id));
            let status = status(turn.started.elapsed(), turn.draft_id);
            let draft = turn.draft_id;
            self.telegram
                .draft(draft, &frame(&head, &text, Some(&status)));
            let Some(segment) = self
                .sessions
                .get_mut(&id)
                .and_then(|s| s.turn.as_mut())
                .and_then(|t| t.segment.as_mut())
            else {
                continue;
            };
            segment.framed = Some((text, Instant::now()));
        }
    }

    /// A message from the chat. What it replies to says where it goes: a message from a
    /// session reaches that session, and the message `/new` left behind opens a
    /// conversation in the directory it names. A message replying to nothing goes to
    /// the session heard from last.
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
        let Some(carrier) = message["message_id"].as_i64() else {
            return;
        };
        let ask = Ask {
            text: text.to_owned(),
            message: carrier,
        };
        let replied = &message["reply_to_message"];
        match address(replied) {
            Some(address) if address == NEW => match body(replied) {
                Some(cwd) => self.open(Path::new(&cwd), ask),
                None => self.say("that anchor names no directory"),
            },
            Some(address) => self.send(&address, ask),
            None => match self.latest() {
                Some(address) => self.send(&address, ask),
                None => self.say("no session is running here; `/new <directory>` opens one"),
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
    fn anchor(&mut self, argument: &str) {
        let Some(cwd) = expand(argument) else {
            self.say("`/new <directory>`");
            return;
        };
        if !cwd.is_dir() {
            self.say(&format!("`{}` is not a directory", cwd.display()));
            return;
        }
        let head = hook::head(&hook::project(&cwd), NEW, None);
        let message = hook::compose(&head, "", "", &cwd.to_string_lossy());
        self.telegram.send(&message, Sound::Silent, None);
    }

    /// Opens a window for a conversation and keeps its first prompt until the session
    /// there reports that it is ready.
    fn open(&mut self, cwd: &Path, ask: Ask) {
        if let Err(error) = tmux::open(cwd) {
            self.say(&format!("tmux: {error}"));
            return;
        }
        self.opening.push((cwd.to_owned(), ask));
    }

    /// Types into the session whose id starts with `address`.
    fn send(&mut self, address: &str, ask: Ask) {
        let Some((id, session)) = self.sessions.iter().find(|(id, _)| id.starts_with(address))
        else {
            self.say(&format!("`{address}` is not a session running here"));
            return;
        };
        let id = id.clone();
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
        if let Err(error) = pane.deliver(&ask.text) {
            self.say(&format!("tmux: {error}"));
            return;
        }
        self.sessions
            .get_mut(&id)
            .expect("the session just found")
            .asked
            .push_back(ask);
    }

    fn say(&self, text: &str) {
        self.telegram.send(text, Sound::Silent, None);
    }
}

/// The chat message that carried a prompt, when klaude is the one that typed it.
/// Anything asked before the match never reached a prompt, so it goes with the match.
fn pair(asked: &mut VecDeque<Ask>, prompt: &str) -> Option<i64> {
    let at = asked.iter().position(|ask| ask.text == prompt)?;
    asked.drain(..=at).next_back().map(|ask| ask.message)
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

/// The same across a turn's frames so they animate into each other, non-zero as Telegram
/// requires, and different per turn so one turn's draft is not the next one's.
fn draft_id(message_id: &str) -> i64 {
    let mut hasher = DefaultHasher::new();
    message_id.hash(&mut hasher);
    i64::try_from(hasher.finish() & 0x7fff_ffff).expect("31 bits fit") | 1
}

/// One frame of a turn's draft. Telegram offers no way to retire a draft, and sending a
/// message leaves it standing, so a segment that has become a message is framed out of
/// the draft to keep the same words from being on screen twice.
fn frame(head: &str, text: &str, status: Option<&str>) -> String {
    match status {
        Some(status) => format!("{head}\n\n{text}\n<tg-thinking>{status}</tg-thinking>"),
        None => format!("{head}\n\n{text}"),
    }
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
    fn a_draft_id_is_stable_per_turn_and_never_zero() {
        assert_eq!(draft_id("turn_1"), draft_id("turn_1"));
        assert_ne!(draft_id("turn_1"), draft_id("turn_2"));
        assert!(draft_id("").is_positive());
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
            subject: subject.to_owned(),
            outcome,
        }
    }

    #[test]
    fn a_call_reads_as_its_tool_its_subject_and_how_it_went() {
        assert_eq!(
            call("Read", "src/listen.rs", Outcome::Running).line(),
            "○ Read `src/listen.rs`"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_millis(1400))
            )
            .line(),
            "● Bash `cargo test` 1s"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_millis(12))
            )
            .line(),
            "● Bash `cargo test` 12ms"
        );
        // What a failure reported reads on a line of its own.
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Failed(Duration::from_secs(4), "Exit code 1".to_owned())
            )
            .line(),
            "× Bash `cargo test` 4s  \n⎿ Exit code 1"
        );
        let subagent = Call {
            agent: Some("Explore".to_owned()),
            ..call("Grep", "fn seal", Outcome::Running)
        };
        assert_eq!(subagent.line(), "○ [Explore] Grep `fn seal`");
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
        assert_eq!(listed, "○ Read `src/listen.rs`  \n● Bash `cargo test` 1s");
    }

    #[test]
    fn a_long_run_counts_the_calls_it_stops_listing() {
        let calls: Vec<Call> = (0..RUN_MAX + 3)
            .map(|index| call("Read", &format!("file{index}"), Outcome::Running))
            .collect();
        let listed = listing(&calls);
        assert!(listed.contains("… 3 earlier"), "the run reads {listed}");
        assert!(
            !listed.contains("Read `file2`"),
            "the third call is still listed"
        );
        assert!(
            listed.contains("Read `file3`"),
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
            message,
        };
        let mut asked = VecDeque::from([ask("first", 1), ask("second", 2)]);
        assert_eq!(pair(&mut asked, "second"), Some(2));
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

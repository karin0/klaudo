//! The one process per machine that owns every Telegram call of every session, and the
//! only reader of the chat. It holds the calls in order per turn, and it is where a
//! message from the chat becomes keystrokes in a session's terminal.

mod chat;
mod render;
mod turn;
mod usage;

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kuriero::{CallbackQuery, Message};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::hook::{self, Event};
use crate::telegram::{Place, Sound, Telegram};
use crate::tmux::Pane;

use chat::{COMMANDS, tilde};
use render::{code, took};
use turn::{Ask, SETTLE, Thread, Turn};
use usage::{Limits, Status, Window};

/// How long a session killed mid-turn keeps its message showing the turn as running.
/// A session that exits on its own says so, and a message from the chat checks every
/// session before it is routed.
const SWEEP: Duration = Duration::from_secs(5);
/// Long enough to let a restarting instance take over from one still shutting down.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(50);
/// After a rejected poll, before asking again.
const RETRY: Duration = Duration::from_secs(5);
/// A send past the socket's own limit fails, and the hook falls back to reporting the
/// event itself, so this only has to cover a clamped message with room to spare.
const DATAGRAM_MAX: usize = 200 * 1024;
/// How many exited sessions a reply can still resume, oldest forgotten first. An entry is
/// an id, a directory and the start of a prompt, so the list stays within a few hundred
/// kilobytes.
const ENDED_MAX: usize = 1000;

/// Where the daemon's socket lives. `$XDG_RUNTIME_DIR` belongs to this user alone, so
/// without it there is no daemon to reach.
pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|base| PathBuf::from(base).join("klaudo"))
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
/// `klaudo send` asking where a file goes. That question waits for the answer at the
/// abstract address `reply`, and names no session when it ran outside Claude Code, which
/// leaves `cwd` to say where the file goes.
#[derive(Deserialize)]
#[serde(untagged)]
enum Arrival {
    Hook(Box<Handoff>),
    Press {
        press: CallbackQuery,
    },
    Status {
        status: Status,
    },
    Chat {
        message: Message,
    },
    Locate {
        session: Option<String>,
        cwd: PathBuf,
        reply: String,
    },
}

/// Where the files `klaudo send` uploads go, and the caption that addresses each of them.
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

    let running_file = directory.join("state.json");
    // A list a different version wrote may not parse, and it only saves the sessions
    // from waiting for their next event, so that costs a warning.
    let running: Vec<Running> = load(&running_file).unwrap_or_else(|error| {
        eprintln!("{}: {error}", running_file.display());
        Vec::new()
    });
    // The next save would overwrite a record that does not parse.
    let kept = crate::xdg_home("XDG_STATE_HOME", ".local/state").join("klaudo");
    fs::create_dir_all(&kept).expect("state directory");
    let ended_file = kept.join("ended.json");
    let ended =
        load(&ended_file).unwrap_or_else(|error| panic!("{}: {error}", ended_file.display()));
    let mut machine = Machine {
        telegram: Telegram::new(),
        answers,
        sessions: running
            .into_iter()
            .map(|Running { known, pid, pane }| {
                let mut session = Session::new(known.dir, pid, pane, known.seen);
                session.trail = known.trail;
                (known.id, session)
            })
            .collect(),
        opening: Vec::new(),
        ended,
        swept: Instant::now(),
        running_file,
        ended_file,
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
    let started =
        i64::try_from(now_millis() / 1000).expect("any u64 of milliseconds fits an i64 in seconds");
    let socket = UnixDatagram::unbound().expect("socket");
    let mut offset = 0;
    loop {
        let Some(updates) = telegram.updates(offset) else {
            std::thread::sleep(RETRY);
            continue;
        };
        for update in updates {
            offset = update.id + 1;
            // A press only redraws a menu or posts an anchor, so a backlog of them
            // replays nothing a terminal would take.
            let handoff = match (update.callback_query, update.message) {
                (Some(press), _) => serde_json::json!({"press": press}),
                (None, Some(message)) if message.date >= started => {
                    serde_json::json!({"message": message})
                }
                _ => continue,
            }
            .to_string();
            if let Err(error) = socket.send_to(handoff.as_bytes(), target) {
                eprintln!("forward: {error}");
            }
        }
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
    /// What the daemon has typed into this session and not yet seen reported as a
    /// prompt.
    asked: VecDeque<Ask>,
    turn: Option<Turn>,
    /// The turn that finished most recently, so its stragglers do not open it again.
    done: Option<String>,
    /// When this session was last heard from, which is what an unaddressed message from
    /// the chat is delivered by, in Unix milliseconds.
    seen: u64,
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
    fn new(dir: PathBuf, pid: u32, pane: Option<Pane>, seen: u64) -> Self {
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
    /// Where `klaudo send` hears where its file goes.
    answers: UnixDatagram,
    sessions: BTreeMap<String, Session>,
    opening: Vec<Opening>,
    /// Every exited session, oldest first.
    ended: VecDeque<Known>,
    swept: Instant,
    /// Where the running sessions are written. A reboot clears it, along with the
    /// processes and panes it names.
    running_file: PathBuf,
    /// Where `ended` is written, which outlives a reboot.
    ended_file: PathBuf,
    /// The limits a status line last reported, and when, in Unix seconds.
    limits: Option<(Limits, u64)>,
}

/// A session the daemon has heard from: where it ran, which is where a reply to it
/// resumes it once it has exited, and when it was last heard from, which is how recent
/// its project is.
#[derive(Serialize, Deserialize, Clone)]
struct Known {
    id: String,
    dir: PathBuf,
    /// Unix milliseconds.
    seen: u64,
    trail: Trail,
}

/// A running session, with the process and the pane a message from the chat is typed
/// into. What the daemon saves of it outlives a restart, so a session idle through one
/// is still reachable, while a turn in flight and what the chat asked of a window are
/// left behind.
#[derive(Serialize, Deserialize)]
struct Running {
    #[serde(flatten)]
    known: Known,
    pid: u32,
    pane: Option<Pane>,
}

/// A file not written yet holds nothing.
fn load<T: DeserializeOwned + Default>(path: &Path) -> serde_json::Result<T> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => panic!("{}: {error}", path.display()),
    }
}

/// Renamed into place, so a daemon killed mid-write leaves the last version whole.
fn store(path: &Path, value: &impl Serialize) {
    let written = path.with_extension("tmp");
    let raw = serde_json::to_vec(value).expect("the state serializes");
    if let Err(error) = fs::write(&written, raw).and_then(|()| fs::rename(&written, path)) {
        eprintln!("save {}: {error}", path.display());
    }
}

/// The time now in Unix milliseconds.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).expect("a date before 2^64 ms")
        })
}

/// What the chat asked, waiting for the session of the window the daemon opened for it.
struct Opening {
    dir: PathBuf,
    /// The session the window resumes, which is what the ask waits for. A new
    /// conversation waits for the next session to start in `dir`.
    resume: Option<String>,
    pane: Pane,
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
            let before = self.ended.len();
            self.ended.retain(|ended| ended.id != id);
            if self.ended.len() < before {
                self.save_ended();
            }
        }
        let session = self.sessions.entry(id.clone()).or_insert_with(|| {
            Session::new(PathBuf::from(&event.cwd), pid, pane.clone(), now_millis())
        });
        session.pid = pid;
        session.pane = pane;
        session.seen = now_millis();
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
        let running: Vec<_> = self
            .sessions
            .iter()
            .map(|(id, session)| Running {
                known: Known {
                    id: id.clone(),
                    dir: session.dir.clone(),
                    seen: session.seen,
                    trail: session.trail.clone(),
                },
                pid: session.pid,
                pane: session.pane.clone(),
            })
            .collect();
        store(&self.running_file, &running);
    }

    /// Written only when `ended` changes, since it is on disk and far larger than the
    /// running sessions.
    fn save_ended(&self) {
        store(&self.ended_file, &self.ended);
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
            return Err(format!("session {id} has not reported to klaudo"));
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
    /// the sweep for a session killed mid-turn or a window closed before its session
    /// started. With none, only an arrival wakes it.
    fn due(&self) -> Option<Instant> {
        let turns = || {
            self.sessions
                .values()
                .filter_map(|session| session.turn.as_ref())
        };
        let sweep =
            (turns().next().is_some() || !self.opening.is_empty()).then(|| self.swept + SWEEP);
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
        let closed: Vec<Opening> = self
            .opening
            .extract_if(.., |opening| !opening.pane.open())
            .collect();
        for opening in closed {
            self.say(
                opening.ask.place,
                &format!(
                    "the window opened in {} closed before its session started",
                    code(&tilde(&opening.dir))
                ),
            );
        }
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
        self.ended.push_back(Known {
            id: id.to_owned(),
            dir: session.dir,
            seen: session.seen,
            trail: session.trail,
        });
        if self.ended.len() > ENDED_MAX {
            self.ended.pop_front();
        }
        self.save_ended();
    }

    fn say(&self, place: Place, text: &str) {
        self.telegram.send(place, text, Sound::Silent, None);
    }
}

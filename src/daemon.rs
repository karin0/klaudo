use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Error, ErrorKind};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::hook::{self, Event};
use crate::telegram::{Sound, Telegram};

/// A draft disappears 30 seconds after its last frame, so a turn that goes quiet inside
/// a long tool call still needs frames to keep it on screen.
const REFRESH: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(200);
/// A session killed before it reaches Stop leaves nobody to shut the daemon down.
const IDLE_LIMIT: Duration = Duration::from_secs(3600);
/// Long enough for the previous turn's daemon to finish its last call and let go.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(50);
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
/// turn itself, so this only has to cover a clamped message with room to spare.
const DATAGRAM_MAX: usize = 200 * 1024;

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(base).join("klaude")
}

pub fn socket_path(session: &str) -> PathBuf {
    runtime_dir().join(format!("{session}.sock"))
}

pub fn log_path(session: &str) -> PathBuf {
    runtime_dir().join(format!("{session}.log"))
}

/// Owns every Telegram call of one turn, so the draft frames and the message that
/// replaces them are ordered by being issued from the same place.
pub fn run(session: &str, cwd: &str) {
    let directory = runtime_dir();
    fs::create_dir_all(&directory).expect("runtime directory");
    let lock = File::create(directory.join(format!("{session}.lock"))).expect("lock file");
    if !acquire(&lock) {
        return;
    }

    let path = socket_path(session);
    let _ = fs::remove_file(&path);
    let socket = UnixDatagram::bind(&path).expect("bind");
    socket.set_read_timeout(Some(POLL)).expect("read timeout");

    let mut turn = Turn::new(hook::project(cwd), session.to_owned());
    let mut buffer = vec![0u8; DATAGRAM_MAX];
    loop {
        match socket.recv(&mut buffer) {
            Ok(size) => {
                let event = serde_json::from_slice(&buffer[..size]).expect("hook input is JSON");
                if turn.handle(&event) {
                    break;
                }
            }
            Err(error) if quiet(&error) => {}
            Err(error) => panic!("recv: {error}"),
        }
        turn.tick();
        if turn.idle() > IDLE_LIMIT {
            // A session killed mid-turn never sends Stop, and what it did say still goes.
            turn.seal();
            break;
        }
    }
    let _ = fs::remove_file(&path);
}

fn quiet(error: &Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn acquire(lock: &File) -> bool {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return true,
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => panic!("lock: {error}"),
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(LOCK_RETRY);
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

struct Turn {
    telegram: Telegram,
    project: String,
    session: String,
    prompt: Option<String>,
    started: Instant,
    touched: Instant,
    segment: Option<Segment>,
}

impl Turn {
    fn new(project: String, session: String) -> Self {
        let now = Instant::now();
        Self {
            telegram: Telegram::from_env(),
            project,
            session,
            prompt: None,
            started: now,
            touched: now,
            segment: None,
        }
    }

    fn idle(&self) -> Duration {
        self.touched.elapsed()
    }

    fn head(&self) -> String {
        hook::head(&self.project, &self.session, self.prompt.as_deref())
    }

    /// True once the turn has been reported and nothing is left to send.
    fn handle(&mut self, event: &Event) -> bool {
        self.touched = Instant::now();
        // The base hook schema leaves this optional, so it is taken from whichever event
        // of the turn carries it first.
        if self.prompt.is_none() {
            self.prompt.clone_from(&event.prompt_id);
        }
        match event.hook_event_name.as_str() {
            "MessageDisplay" => {
                self.delta(event);
                false
            }
            "Stop" | "StopFailure" => {
                self.finish(event);
                true
            }
            _ => {
                self.telegram
                    .send(&hook::message(event, &self.head(), ""), Sound::Ring);
                false
            }
        }
    }

    fn delta(&mut self, event: &Event) {
        let (Some(id), Some(index), Some(delta)) = (&event.message_id, event.index, &event.delta)
        else {
            return;
        };
        if self.segment.as_ref().is_none_or(|open| open.id != *id) {
            self.seal();
            self.segment = Some(Segment::new(id));
        }
        let segment = self.segment.as_mut().expect("just placed");
        // Three hook processes run at once, so a delta can arrive ahead of its predecessor.
        segment.chunks.insert(index, delta.clone());
    }

    /// A segment that has stopped receiving text is complete, so what its draft was
    /// showing becomes a message. The turn interrupts once, at its end, so this is quiet.
    fn seal(&mut self) {
        let Some(segment) = self.segment.take() else {
            return;
        };
        let text = segment.text();
        if text.is_empty() {
            return;
        }
        // The tag marks a finished turn, and this segment is the middle of one.
        let posted = hook::compose(&self.head(), &took(self.started.elapsed()), "", &text);
        self.telegram.send(&posted, Sound::Silent);
    }

    fn tick(&mut self) {
        if self.segment.is_none() {
            return;
        }
        let head = self.head();
        let elapsed = self.started.elapsed();
        let segment = self.segment.as_mut().expect("checked just above");
        let text = segment.text();
        if text == segment.framed && segment.frame_at.elapsed() < REFRESH {
            return;
        }
        let status = status(elapsed, segment.draft_id);
        let frame = format!("{head}\n\n{text}\n<tg-thinking>{status}</tg-thinking>");
        self.telegram.draft(segment.draft_id, &frame);
        segment.framed = text;
        segment.frame_at = Instant::now();
    }

    fn finish(&mut self, event: &Event) {
        // The last segment's text is what this event carries, so its draft is dropped
        // rather than posted a second time just above the message that repeats it.
        self.segment = None;
        let message = hook::message(event, &self.head(), &took(self.started.elapsed()));
        // The one sound of the turn: the reply is complete and worth coming back to.
        self.telegram.send(&message, Sound::Ring);
    }
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
}

//! A turn, from the prompt that starts it to the answer that closes it, as the
//! messages the chat shows of it.

use std::collections::{BTreeMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use crate::hook::{self, Event};
use crate::telegram::{MAX_CHARS, Place, Sound};

use super::Machine;
use super::render::{Call, Outcome, first_line, listing, took};
use super::usage::status_line;

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
/// How often a running turn shows its session typing, within the five seconds Telegram
/// shows a chat action for.
const TYPING: Duration = Duration::from_secs(4);
/// How long a tool call waits before it is filed, so words arriving within that time
/// stand above it, and how long an open segment's text stays quiet before the message
/// showing it is rewritten, so a `Stop` arriving milliseconds behind its answer takes
/// over and the answer does not stand in the chat twice.
pub(super) const SETTLE: Duration = Duration::from_millis(100);

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

/// What a message carries around a segment's text, its title and status line, with room
/// to spare.
const FRAME: usize = 1024;
/// How much of a session's latest prompt its button in a `/resume` menu shows.
const PROMPT_MAX: usize = 40;
/// How much of the first line of what a failed tool reported its call shows, past which
/// it stops reading at a glance.
const WHY_MAX: usize = 60;

/// One stretch of a turn: an assistant message and the run of tool calls it goes on to
/// make. It is written into the turn's open message until the next one starts and that
/// message is its own. A turn opens with an empty segment, so what stands in the chat
/// while nothing has been said is the status line.
struct Segment {
    calls: Vec<Call>,
    /// The assistant message's id and its deltas by index, because three hook processes
    /// run at once and one can arrive ahead of its predecessor.
    said: Option<(String, BTreeMap<u32, String>)>,
    /// What the open message was last written with and when, absent until it exists.
    written: Option<(String, Instant)>,
    /// The chat and message this segment finished in and the elapsed time stamped on
    /// it, which is what a flush or an outcome arriving later rewrites. The turn may
    /// have moved to another chat since.
    posted: Option<(i64, i64, Duration)>,
    /// When the segment last received text.
    heard: Instant,
}

impl Segment {
    fn new() -> Self {
        Self {
            calls: Vec::new(),
            said: None,
            written: None,
            posted: None,
            heard: Instant::now(),
        }
    }

    /// True for the assistant message or tool call `id` names.
    fn holds(&self, id: &str) -> bool {
        self.said.as_ref().is_some_and(|(said, _)| said == id)
            || self.calls.iter().any(|call| call.id == id)
    }

    /// The words, then the run they introduce.
    fn text(&self) -> String {
        let words: String = self
            .said
            .iter()
            .flat_map(|(_, chunks)| chunks.values().map(String::as_str))
            .collect();
        match (words.as_str(), self.calls.as_slice()) {
            (_, []) => words,
            ("", calls) => listing(calls),
            (_, calls) => format!("{words}\n\n{}", listing(calls)),
        }
    }
}

/// The place a turn is posted in and the message there carrying what was asked, which
/// the rest of the turn replies to. A turn asked from the phone stays in the place it was
/// asked in, and any other goes to the session's home.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Thread {
    pub(super) place: Place,
    pub(super) prompt: Option<i64>,
}

/// Text from the chat and the message that carried it, which is the thread the turn it
/// becomes replies into.
pub(super) struct Ask {
    pub(super) text: String,
    pub(super) place: Place,
    pub(super) message: i64,
}

/// One turn, from the event that started it to its Stop.
pub(super) struct Turn {
    pub(super) prompt_id: String,
    /// Where this turn starts in the list of status words.
    seed: u64,
    pub(super) started: Instant,
    pub(super) thread: Thread,
    /// The message at the foot of the turn, carrying the open segment and the status
    /// line. The segment that finishes in it takes it, and the next one opens another.
    pub(super) live: Option<i64>,
    segment: Option<Segment>,
    /// The segments the chat already has, kept because what belongs in one goes on
    /// arriving after it was posted.
    sealed: Vec<Segment>,
    /// Tool calls announced but not filed yet, oldest first. A call the turn ended on
    /// is dropped with the turn: the answer is what that message is for.
    pub(super) pending: Vec<(Instant, Call)>,
    /// When the chat is next shown the session typing. A session waiting on a dialog is
    /// not typing.
    pub(super) typing: Option<Instant>,
}

impl Turn {
    /// Goes on in `thread`, leaving what it posted where it is. The message showing the
    /// open segment is posted again there, so the one it was in, as `(chat, message)`,
    /// is for the caller to take back.
    pub(super) fn relocate(&mut self, thread: Thread) -> Option<(i64, i64)> {
        let left = self.live.take().map(|live| (self.thread.place.chat, live));
        self.thread = thread;
        if let Some(segment) = self.segment.as_mut() {
            segment.written = None;
        }
        left
    }

    /// When the message showing the open segment is due to be written next, which is
    /// only to move its clock while the chat already shows what the segment says.
    pub(super) fn due(&self) -> Option<Instant> {
        let segment = self.segment.as_ref()?;
        let quiet = segment.heard + SETTLE;
        match &segment.written {
            None => Some((self.started + REWRITE).max(quiet)),
            Some((written, at)) if *written == segment.text() => Some(*at + REFRESH),
            Some((_, at)) => Some((*at + rewrite(self.thread.place.chat)).max(quiet)),
        }
    }

    /// The run a tool call belongs to, which is the open one until the turn moves past
    /// it and the call reports from the chat.
    fn holding(&mut self, tool_use_id: &str) -> Option<&mut Segment> {
        self.segment
            .iter_mut()
            .chain(self.sealed.iter_mut())
            .find(|segment| segment.calls.iter().any(|call| call.id == tool_use_id))
    }
}

impl Machine {
    fn turn_mut(&mut self, id: &str) -> Option<&mut Turn> {
        self.sessions.get_mut(id)?.turn.as_mut()
    }

    /// Writes `text` into a turn's live message, or posts it in the turn's thread while
    /// there is none. A message the bot posts clears its typing, which is shown again at
    /// once.
    fn post_live(
        &mut self,
        id: &str,
        thread: Thread,
        live: Option<i64>,
        text: &str,
    ) -> Option<i64> {
        if let Some(message) = live {
            self.telegram.edit(thread.place.chat, message, text);
            return Some(message);
        }
        let posted = self
            .telegram
            .send(thread.place, text, Sound::Silent, thread.prompt);
        if let Some(turn) = self.turn_mut(id)
            && turn.typing.is_some()
        {
            turn.typing = Some(Instant::now());
        }
        posted
    }

    pub(super) fn submitted(&mut self, id: &str, event: &Event) {
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
        // A prompt the daemon typed is already in the chat as the message that asked
        // for it, and that message is what the turn replies to.
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
        session.trail.prompt = first_line(event.prompt.as_deref().unwrap_or_default(), PROMPT_MAX);
        session.left(thread.place, thread.prompt);
        if running {
            session.queued.push_back(thread);
        } else {
            let prompt_id = event.prompt_id.clone().unwrap_or_default();
            session.turn = Some(Turn {
                segment: Some(Segment::new()),
                seed: seed(&prompt_id),
                prompt_id,
                started: Instant::now(),
                thread,
                live: None,
                sealed: Vec::new(),
                pending: Vec::new(),
                typing: None,
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
            segment: Some(Segment::new()),
            sealed: Vec::new(),
            pending: Vec::new(),
            typing: None,
        });
        true
    }

    pub(super) fn delta(&mut self, id: &str, event: &Event) {
        let (Some(message_id), Some(index), Some(delta)) =
            (&event.message_id, event.index, &event.delta)
        else {
            return;
        };
        if !self.turn(id, event.prompt_id.as_deref()) {
            return;
        }
        let Some(turn) = self.turn_mut(id) else {
            return;
        };
        if let Some(segment) = turn
            .sealed
            .iter_mut()
            .find(|sealed| sealed.holds(message_id))
        {
            if let Some((_, chunks)) = &mut segment.said {
                chunks.insert(index, delta.clone());
            }
            self.amend(id, message_id);
            return;
        }
        if turn
            .segment
            .as_ref()
            .is_none_or(|open| !open.holds(message_id))
        {
            self.seal(id);
        }
        let Some(turn) = self.turn_mut(id) else {
            return;
        };
        let segment = turn.segment.get_or_insert_with(Segment::new);
        segment
            .said
            .get_or_insert_with(|| (message_id.clone(), BTreeMap::new()))
            .1
            .insert(index, delta.clone());
        segment.heard = Instant::now();
        self.part(id);
    }

    /// A run that would overflow the message of the words introducing it goes on in one
    /// of its own, and the words keep theirs.
    fn part(&mut self, id: &str) {
        let Some(turn) = self.turn_mut(id) else {
            return;
        };
        let Some(segment) = turn
            .segment
            .as_mut()
            .filter(|segment| segment.said.is_some() && !segment.calls.is_empty())
            .filter(|segment| segment.text().chars().count() + FRAME > MAX_CHARS)
        else {
            return;
        };
        let run = Segment {
            calls: std::mem::take(&mut segment.calls),
            ..Segment::new()
        };
        self.seal(id);
        if let Some(turn) = self.turn_mut(id) {
            turn.segment = Some(run);
        }
    }

    /// A tool call waits out `SETTLE` before it is filed, so the words its own message
    /// ends with reach the chat first.
    pub(super) fn calling(&mut self, id: &str, event: &Event) {
        let (Some(tool_use_id), Some(name)) = (&event.tool_use_id, &event.tool_name) else {
            return;
        };
        if !self.turn(id, event.prompt_id.as_deref()) {
            return;
        }
        let Some(turn) = self.turn_mut(id) else {
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

    /// Files the calls that have waited long enough. They join the open segment, under
    /// the words that introduce them, so a turn that talked, worked and talked again
    /// leaves two messages in the chat, the first of them holding the run.
    pub(super) fn place(&mut self, id: &str) {
        let Some(turn) = self.turn_mut(id) else {
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
        turn.segment
            .get_or_insert_with(Segment::new)
            .calls
            .extend(filed);
        self.part(id);
    }

    /// How a call went, which reaches the run it belongs to, and the message that run
    /// was posted in once the turn has moved on.
    pub(super) fn called(&mut self, id: &str, event: &Event) {
        let Some(tool_use_id) = event.tool_use_id.as_deref() else {
            return;
        };
        let took = Duration::from_millis(event.duration_ms.unwrap_or_default());
        let outcome = if event.hook_event_name == "PostToolUseFailure" {
            Outcome::Failed(
                took,
                first_line(event.error.as_deref().unwrap_or("failed"), WHY_MAX),
            )
        } else {
            Outcome::Done(took)
        };
        let Some(turn) = self.turn_mut(id) else {
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
        let Some(call) = segment.calls.iter_mut().find(|call| call.id == tool_use_id) else {
            return;
        };
        call.outcome = outcome;
        if segment.posted.is_some() {
            self.amend(id, tool_use_id);
        }
    }

    /// A segment that has stopped receiving text is complete, so it takes the open
    /// message and the elapsed time it finished at, and the next segment opens another.
    /// A segment that said nothing leaves the open message to the one that follows it.
    pub(super) fn seal(&mut self, id: &str) {
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
        // A segment that ran its course inside one rewrite has no message yet.
        let message = self.post_live(id, thread, live, &done);
        segment.posted = message.map(|message| (thread.place.chat, message, elapsed));
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        session.left(thread.place, message);
        let Some(turn) = session.turn.as_mut() else {
            return;
        };
        turn.sealed.push(segment);
    }

    /// A message the daemon has posted, rewritten with what reached its segment
    /// afterwards. A message's last flushes race the hook of the tool call that ends
    /// it, and a tool reports after the run it belongs to has been left behind, so both
    /// land on a segment the chat already has.
    fn amend(&mut self, id: &str, member: &str) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let Some(turn) = session.turn.as_ref() else {
            return;
        };
        let Some(segment) = turn.sealed.iter().find(|sealed| sealed.holds(member)) else {
            return;
        };
        let Some((chat, message, elapsed)) = segment.posted else {
            return;
        };
        let text = segment.text();
        let head = session.head(id, Some(&turn.prompt_id));
        self.telegram.edit(
            chat,
            message,
            &hook::compose(&head, &took(elapsed), "", &text),
        );
    }

    pub(super) fn finish(&mut self, id: &str, event: &Event) {
        if !self.turn(id, event.prompt_id.as_deref()) {
            return;
        }
        // A segment holding a run keeps its message, and only an assistant message is
        // what this event repeats.
        if self
            .sessions
            .get(id)
            .and_then(|s| s.turn.as_ref())
            .and_then(|t| t.segment.as_ref())
            .is_some_and(|segment| !segment.calls.is_empty())
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

    /// A `/compact` the daemon typed never reports as a prompt, so its start is what
    /// tells the chat it was accepted.
    pub(super) fn compacting(&self, id: &str) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        if let Some(ask) = compaction(&session.asked) {
            self.telegram.acknowledge(ask.place.chat, ask.message);
        }
    }

    /// `/compact` runs no turn, so its end is the answer to it and rings like one,
    /// replying to the message that asked for it when the daemon typed it.
    pub(super) fn compacted(&mut self, id: &str, event: &Event) {
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

    /// A `Notification` means the session waits on a dialog, and any later event of the
    /// turn means the dialog was answered.
    pub(super) fn dialog(&mut self, id: &str, open: bool) {
        if let Some(turn) = self.turn_mut(id) {
            turn.typing = if open {
                None
            } else {
                turn.typing.or_else(|| Some(Instant::now()))
            };
        }
    }

    pub(super) fn keep_typing(&mut self, id: &str) {
        let now = Instant::now();
        let Some(turn) = self.turn_mut(id) else {
            return;
        };
        if turn.typing.is_none_or(|at| at > now) {
            return;
        }
        turn.typing = Some(now + TYPING);
        let place = turn.thread.place;
        self.telegram.typing(place);
    }

    /// Anything else a session reports lands in the thread of the turn it happened in.
    pub(super) fn aside(&mut self, id: &str, event: &Event, sound: Sound) {
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

    /// The open segment as the chat should be showing it: what it has said so far, the
    /// status line under that, and the figures closing an answer under both. The message
    /// holding it is sent once the turn has run long enough to be worth watching, and
    /// rewritten as the segment grows.
    pub(super) fn show(&mut self, id: &str) {
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
        let mut footer = status(elapsed, turn.seed);
        if let Some(line) = status_line(session.window.as_ref(), self.limits.as_ref()) {
            footer = format!("{footer}\n\n{line}");
        }
        let shown = hook::compose(&head, &took(elapsed), "", &running(&text, &footer));
        let message = self.post_live(id, thread, live, &shown);
        let Some(turn) = self.turn_mut(id) else {
            return;
        };
        turn.live = message;
        if message.is_some()
            && let Some(segment) = turn.segment.as_mut()
        {
            segment.written = Some((text, Instant::now()));
        }
    }
}

/// The chat message that carried a prompt, when the daemon is the one that typed it.
/// Anything asked before the match never reached a prompt, so it goes with the match.
fn pair(asked: &mut VecDeque<Ask>, prompt: &str) -> Option<Thread> {
    let at = asked.iter().position(|ask| ask.text == prompt)?;
    asked.drain(..=at).next_back().map(|ask| Thread {
        place: ask.place,
        prompt: Some(ask.message),
    })
}

/// The `/compact` the daemon typed into a session and Claude Code has yet to finish.
fn compaction(asked: &VecDeque<Ask>) -> Option<&Ask> {
    asked.iter().find(|ask| {
        ask.text
            .strip_prefix("/compact")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    })
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
        format!("{text}\n\n{status}")
    }
}

/// The word changes once per refresh, so a turn sitting in a long tool call keeps
/// showing a line that differs from the last one.
pub(super) fn status(elapsed: Duration, seed: u64) -> String {
    let step = seed + elapsed.as_secs() / REFRESH.as_secs();
    let word = WORDS[usize::try_from(step).expect("a turn's seconds") % WORDS.len()];
    format!("✻ {word}… ({})", took(elapsed).trim())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut segment = Segment::new();
        let (_, chunks) = segment.said.insert(("msg_1".to_owned(), BTreeMap::new()));
        chunks.insert(2, "third".to_owned());
        chunks.insert(0, "first ".to_owned());
        chunks.insert(1, "second ".to_owned());
        assert_eq!(segment.text(), "first second third");
        // The run the words introduce stands under them.
        segment.calls.push(Call {
            id: "toolu_1".to_owned(),
            agent: None,
            name: "Read".to_owned(),
            description: String::new(),
            subject: "a".to_owned(),
            outcome: Outcome::Done(Duration::from_secs(1)),
        });
        assert_eq!(
            segment.text(),
            "first second third\n\n● **Read**  `a` **1s**"
        );
    }

    const HERE: Place = Place {
        chat: 7,
        topic: None,
    };

    #[test]
    fn a_prompt_klaudo_typed_is_paired_with_the_message_that_asked_for_it() {
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
    fn a_compact_klaudo_typed_is_found_with_or_without_instructions() {
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
}

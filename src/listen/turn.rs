//! A turn, from the prompt that starts it to the answer that closes it, as the
//! messages the chat shows of it.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::changes::{self, Start, Summary};
use crate::hook::{self, Event, Produced};
use kuriero::Message;

use crate::telegram::{self, Admitted, MAX_BLOCKS, MAX_BYTES, Place, Post, Sound};
use crate::web::Links;

use super::Machine;
use super::chat::{NEW, unreminded};
use super::render::{
    ARGUMENTS_MAX, Call, Outcome, RESULT_MAX, SUMMARY_MAX, first_line, listing, took,
};
use super::transcript::{self, Transcript};
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
/// What `/stop` reaching a session without a running turn is answered with.
const NOTHING_RUNS: &str = "no turn is running in that conversation";
/// How long a turn sent Escape has to show up interrupted in its transcript before the
/// chat hears that it did not stop.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// How long the answer closing a turn waits for the transcript to record the turn's end,
/// since Claude Code writes the turn's last call only along with it, after the Stop hooks
/// have run. The figures under the answer leave that call out otherwise.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// How often a transcript is read while a turn waits on it.
pub(super) const POLL: Duration = Duration::from_millis(50);
/// What Claude Code appends to the transcript as a user message when a turn is
/// interrupted, ending in ` for tool use]` when it cut a tool call short. No hook reports
/// an interruption, and the text is Claude Code's internal format.
const INTERRUPTED: &str = "[Request interrupted by user";

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

/// What a message carries around a segment's text, its title and status line, in bytes
/// and blocks with room to spare.
const FRAME: usize = 1024;
const FRAME_BLOCKS: usize = 4;
/// How much of a session's latest prompt its button in a `/resume` menu shows.
const PROMPT_MAX: usize = 40;

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
    /// The message this segment finished in, which is what a flush or an outcome
    /// arriving later rewrites.
    posted: Option<Posted>,
    /// When the segment last received text.
    heard: Instant,
}

/// A message a segment finished in.
#[derive(Clone, Copy)]
struct Posted {
    /// The turn may have moved to another chat since.
    chat: i64,
    message: i64,
    /// The elapsed time stamped on it.
    elapsed: Duration,
    /// A markdown file, which a rewrite replaces whole.
    filed: bool,
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

    /// The words, then the run they introduce, with the calls a reader follows in view
    /// while the segment is `live`.
    fn text(&self, live: bool, links: Option<Links>) -> String {
        let words = telegram::rich_markdown(
            &self
                .said
                .iter()
                .flat_map(|(_, chunks)| chunks.values().map(String::as_str))
                .collect::<String>(),
            links,
        );
        match (words.as_str(), self.calls.as_slice()) {
            (_, []) => words,
            ("", calls) => listing(calls, live),
            (_, calls) => format!("{words}\n\n{}", listing(calls, live)),
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

/// The answer that closes a turn, waiting for the transcript to record the turn's end.
pub(super) struct Closing {
    answer: Post,
    thread: Thread,
    /// The message showing the turn running, which the answer replaces.
    live: Option<i64>,
    until: Instant,
}

/// Text from the chat and the message that carried it, which is the thread the turn it
/// becomes replies into.
pub(super) struct Ask {
    pub(super) text: String,
    /// The files sent with the text, saved on this machine.
    pub(super) files: Vec<PathBuf>,
    pub(super) place: Place,
    pub(super) message: i64,
    /// The name of the guest who sent the text, which is `None` for the user.
    pub(super) guest: Option<String>,
}

impl Ask {
    /// The text followed by a line per file. Claude Code turns a paste that is one
    /// image's path alone into an attachment, which leaves the prompt it reports
    /// unlike the text typed, so each path goes in a code span.
    pub(super) fn prompt(&self) -> String {
        let paths = self
            .files
            .iter()
            .map(|path| format!("`{}`", path.display()));
        std::iter::once(self.text.clone())
            .filter(|text| !text.is_empty())
            .chain(paths)
            .collect::<Vec<_>>()
            .join("\n")
    }
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
    /// When the chat is next shown the session typing, unless it waits on a dialog.
    pub(super) typing: Instant,
    /// Whether the session waits on a dialog, which the Enter after a reply would answer
    /// with its default choice.
    pub(super) dialog: bool,
    stopping: Option<Stopping>,
    /// Where the turn started in the session's working tree, `None` outside one.
    start: Option<Start>,
}

/// Escape sent to a turn's pane, until its transcript shows the turn interrupted.
struct Stopping {
    /// How long the transcript was then, so an earlier interruption is left out.
    from: u64,
    until: Instant,
}

impl Turn {
    fn new(prompt_id: String, thread: Thread, dir: &Path) -> Self {
        Self {
            start: changes::start(dir),
            segment: Some(Segment::new()),
            seed: seed(&prompt_id),
            prompt_id,
            started: Instant::now(),
            thread,
            live: None,
            sealed: Vec::new(),
            pending: Vec::new(),
            typing: Instant::now(),
            dialog: false,
            stopping: None,
        }
    }

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
    pub(super) fn due(&self, links: Option<Links>) -> Option<Instant> {
        let segment = self.segment.as_ref()?;
        let quiet = segment.heard + SETTLE;
        match &segment.written {
            None => Some((self.started + REWRITE).max(quiet)),
            Some((written, at)) if *written == segment.text(true, links) => Some(*at + REFRESH),
            Some((_, at)) => Some((*at + rewrite(self.thread.place.chat)).max(quiet)),
        }
    }

    /// When the transcript of a turn sent Escape is next read.
    pub(super) fn stop_due(&self) -> Option<Instant> {
        self.stopping.as_ref().map(|_| Instant::now() + POLL)
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

    /// Writes `post` into a turn's live message, or posts it in the
    /// turn's thread while there is none. A post past a rich message is a complete
    /// segment, which goes as a markdown file in place of the live message. A message
    /// the bot posts clears its typing, which is shown again at once.
    fn post_live(
        &mut self,
        id: &str,
        thread: Thread,
        live: Option<i64>,
        post: &Post,
    ) -> Option<i64> {
        let posted = if !telegram::fits(&post.markdown) {
            let filed = self
                .telegram
                .post(thread.place, post, Sound::Silent, thread.prompt);
            if filed.is_some()
                && let Some(live) = live
            {
                self.telegram.delete(thread.place.chat, live);
            }
            filed
        } else if let Some(message) = live {
            self.telegram
                .edit(thread.place.chat, message, &post.markdown);
            return Some(message);
        } else {
            self.telegram
                .send(thread.place, &post.markdown, Sound::Silent, thread.prompt)
        };
        if let Some(turn) = self.turn_mut(id)
            && !turn.dialog
        {
            turn.typing = Instant::now();
        }
        posted
    }

    pub(super) fn submitted(&mut self, id: &str, event: &Event) {
        // The answer to the turn before stands above the prompt.
        self.close(id, true);
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
            let message = hook::message(event, &head, "", Links::new(self.app, id));
            let place = session.home(&self.telegram);
            // The phone's owner asked this, so it arrives without a sound.
            Thread {
                place,
                prompt: self.telegram.post(place, &message, Sound::Silent, None),
            }
        };
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        session.trail.prompt = first_line(
            unreminded(event.prompt.as_deref().unwrap_or_default()),
            PROMPT_MAX,
        );
        session.left(thread.place, thread.prompt);
        if running {
            session.queued.push_back(thread);
        } else {
            session.turn = Some(Turn::new(
                event.prompt_id.clone().unwrap_or_default(),
                thread,
                &session.dir,
            ));
        }
    }

    /// Opens the turn an event belongs to when the event names one that has not been
    /// seen, which is how a queued prompt's turn begins. False for an event of a turn
    /// that already finished: the three hook processes run at once, so a delta can land
    /// after its own `Stop`, and opening a second turn for it would leave a message
    /// beside the answer showing something else.
    fn turn(&mut self, id: &str, event: &Event) -> bool {
        let Some(session) = self.sessions.get(id) else {
            return false;
        };
        let named = event.prompt_id.as_deref().unwrap_or_default();
        if session.done.as_deref() == Some(named) {
            return false;
        }
        match &session.turn {
            Some(open) if open.prompt_id == named => return true,
            Some(_) => self.seal(id),
            None => self.close(id, true),
        }
        let Some(session) = self.sessions.get_mut(id) else {
            return false;
        };
        let place = session.home(&self.telegram);
        let thread = session.queued.pop_front().unwrap_or(Thread {
            place,
            prompt: None,
        });
        session.turn = Some(Turn::new(named.to_owned(), thread, &session.dir));
        true
    }

    pub(super) fn delta(&mut self, id: &str, event: &Event) {
        let (Some(message_id), Some(index), Some(delta)) =
            (&event.message_id, event.index, &event.delta)
        else {
            return;
        };
        if self.turn(id, event) {
            self.heard(id, message_id, index, delta.clone());
        }
    }

    /// Text of the assistant message `message_id` reaching its segment, which a message
    /// starting opens.
    fn heard(&mut self, id: &str, message_id: &str, index: u32, delta: String) {
        let Some(turn) = self.turn_mut(id) else {
            return;
        };
        if let Some(segment) = turn
            .sealed
            .iter_mut()
            .find(|sealed| sealed.holds(message_id))
        {
            if let Some((_, chunks)) = &mut segment.said {
                chunks.insert(index, delta);
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
            .get_or_insert_with(|| (message_id.to_owned(), BTreeMap::new()))
            .1
            .insert(index, delta);
        segment.heard = Instant::now();
        self.part(id);
    }

    /// A run that would overflow the message of the words introducing it goes on in one
    /// of its own, and the words keep theirs.
    fn part(&mut self, id: &str) {
        let links = Links::new(self.app, id);
        let Some(turn) = self.turn_mut(id) else {
            return;
        };
        let Some(segment) = turn
            .segment
            .as_mut()
            .filter(|segment| segment.said.is_some() && !segment.calls.is_empty())
            .filter(|segment| {
                let text = segment.text(true, links);
                text.len() + FRAME > MAX_BYTES
                    || telegram::blocks(&text) + FRAME_BLOCKS > MAX_BLOCKS
            })
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
        if !self.turn(id, event) {
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
                arguments: event
                    .arguments()
                    .map(|arguments| hook::head(&arguments, usize::MAX, ARGUMENTS_MAX)),
                outcome: Outcome::Running,
            },
        ));
    }

    /// Files the calls that have waited long enough. They join the open segment, under
    /// the words that introduce them, so a turn that talked, worked and talked again
    /// leaves two messages in the chat, the first of them holding the run. Narration
    /// fires no hook, so the transcript is read for the narration above each call, which
    /// opens a segment as the words of an assistant message do.
    pub(super) fn place(&mut self, id: &str) {
        while let Some(call) = self.turn_mut(id).and_then(|turn| {
            turn.pending
                .first()
                .is_some_and(|(at, _)| at.elapsed() >= SETTLE)
                .then(|| turn.pending.remove(0).1)
        }) {
            let narration = self
                .sessions
                .get_mut(id)
                .and_then(|session| session.transcript.as_mut())
                .and_then(|transcript| transcript.narration(&call.id));
            if let Some(narration) = narration {
                self.heard(id, &call.id, 0, narration);
            }
            if let Some(turn) = self.turn_mut(id) {
                turn.segment
                    .get_or_insert_with(Segment::new)
                    .calls
                    .push(call);
            }
            self.part(id);
        }
    }

    /// How a call went, which reaches the run it belongs to, and the message that run
    /// was posted in once the turn has moved on.
    pub(super) fn called(&mut self, id: &str, event: &Event) {
        let Some(tool_use_id) = event.tool_use_id.as_deref() else {
            return;
        };
        let took = Duration::from_millis(event.duration_ms.unwrap_or_default());
        let printed = |text: &str| {
            if event.mcp() {
                hook::head(text, usize::MAX, RESULT_MAX)
            } else {
                hook::head(text, hook::OUTPUT_LINES, hook::OUTPUT_MAX)
            }
        };
        let outcome = if event.hook_event_name == "PostToolUseFailure" {
            Outcome::Failed(took, printed(event.error.as_deref().unwrap_or("failed")))
        } else {
            match event.result() {
                Produced::Printed(text) => Outcome::Printed(took, printed(&text)),
                Produced::Summed(text) => Outcome::Summed(took, first_line(&text, SUMMARY_MAX)),
            }
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
        let text = segment.text(false, Links::new(self.app, id));
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
        segment.posted = message.map(|message| Posted {
            chat: thread.place.chat,
            message,
            elapsed,
            filed: !telegram::fits(&done.markdown),
        });
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
        let Some(posted) = segment.posted else {
            return;
        };
        let head = session.head(id, Some(&turn.prompt_id));
        let post = hook::compose(
            &head,
            &took(posted.elapsed),
            "",
            &segment.text(false, Links::new(self.app, id)),
        );
        let thread = turn.thread;
        if posted.filed {
            self.telegram.refile(posted.chat, posted.message, &post);
            return;
        }
        if telegram::fits(&post.markdown) {
            self.telegram
                .edit(posted.chat, posted.message, &post.markdown);
            return;
        }
        // A rich message cannot become a file, so the file takes its place.
        let Some(filed) = self
            .telegram
            .post(thread.place, &post, Sound::Silent, thread.prompt)
        else {
            return;
        };
        self.telegram.delete(posted.chat, posted.message);
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        session.left(thread.place, Some(filed));
        if let Some(segment) = session
            .turn
            .as_mut()
            .and_then(|turn| turn.sealed.iter_mut().find(|sealed| sealed.holds(member)))
        {
            segment.posted = Some(Posted {
                chat: thread.place.chat,
                message: filed,
                filed: true,
                ..posted
            });
        }
    }

    pub(super) fn finish(&mut self, id: &str, event: &Event) {
        if !self.turn(id, event) {
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
        let links = Links::new(self.app, id);
        let mut answer = hook::message(event, &head, &took(turn.started.elapsed()), links);
        let line = turn.start.as_ref().zip(links).and_then(|(start, links)| {
            review(links, &start.top, &start.head, ".")
                .inspect_err(|error| eprintln!("changes since {}: {error}", start.head))
                .ok()
                .flatten()
        });
        if let Some(line) = line {
            answer.markdown = format!("{}\n\n{line}", answer.markdown);
        }
        let closing = Closing {
            answer,
            thread: turn.thread,
            live: turn.live,
            until: Instant::now() + CLOSE_WAIT,
        };
        // Only Stop is answered with figures, and only it has a turn's end written.
        if event.hook_event_name == "Stop" {
            session.closing = Some(closing);
            self.close(id, false);
        } else {
            self.answer(id, &closing);
        }
    }

    /// Posts the answer waiting to close a turn of session `id`, under the figures of the
    /// turn, once the transcript records its end, `CLOSE_WAIT` has passed, or `now` asks
    /// for it because the session moved on.
    pub(super) fn close(&mut self, id: &str, now: bool) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(closing) = &session.closing else {
            return;
        };
        let written = session.transcript.as_mut().is_none_or(Transcript::closed);
        if !now && !written && closing.until > Instant::now() {
            return;
        }
        let mut closing = session.closing.take().expect("an answer waiting");
        let (hit, spent) = (session.hit(), session.spent());
        if let Some(line) = status_line(session.context(), hit, spent, self.limits.as_ref()) {
            closing.answer.markdown = format!("{}\n\n{line}", closing.answer.markdown);
        }
        self.answer(id, &closing);
    }

    fn answer(&mut self, id: &str, closing: &Closing) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        if let Some(transcript) = &mut session.transcript {
            transcript.ended();
        }
        // The one sound of the turn: the reply is complete and worth coming back to.
        let thread = closing.thread;
        let answer = self
            .telegram
            .post(thread.place, &closing.answer, Sound::Ring, thread.prompt);
        session.left(thread.place, answer);
        // This event carries the last segment's text, so the message that was showing
        // it goes rather than standing above the one that repeats it.
        if let Some(live) = closing.live {
            self.telegram.delete(thread.place.chat, live);
        }
    }

    /// A `/compact` the daemon typed never reports as a prompt, so its start is what
    /// tells the chat it was accepted.
    pub(super) fn compacting(&self, id: &str) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        if let Some(ask) = typed(&session.asked, "compact") {
            self.telegram.acknowledge(ask.place.chat, ask.message);
        }
    }

    /// `/compact` runs no turn, so its end is the answer to it and rings like one,
    /// replying to the message that asked for it when the daemon typed it.
    pub(super) fn compacted(&mut self, id: &str, event: &Event) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let typed = take_typed(&mut session.asked, "compact");
        let head = session.head(id, event.prompt_id.as_deref());
        let thread = typed.unwrap_or_else(|| self.thread(&self.sessions[id]));
        let answer = self.telegram.post(
            thread.place,
            &hook::message(event, &head, "", Links::new(self.app, id)),
            Sound::Ring,
            thread.prompt,
        );
        if let Some(session) = self.sessions.get_mut(id) {
            session.left(thread.place, answer);
        }
    }

    /// A `Notification` of a dialog means the session waits on it, and any later event of
    /// the turn means it was answered.
    pub(super) fn dialog(&mut self, id: &str, open: bool) {
        if let Some(turn) = self.turn_mut(id) {
            if turn.dialog && !open {
                turn.typing = Instant::now();
            }
            turn.dialog = open;
        }
    }

    pub(super) fn keep_typing(&mut self, id: &str) {
        let now = Instant::now();
        let Some(turn) = self.turn_mut(id) else {
            return;
        };
        if turn.dialog || turn.typing > now {
            return;
        }
        turn.typing = now + TYPING;
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
        let message = hook::message(event, &head, "", Links::new(self.app, id));
        let posted = self
            .telegram
            .post(thread.place, &message, sound, thread.prompt);
        if let Some(session) = self.sessions.get_mut(id) {
            session.left(thread.place, posted);
        }
    }

    /// The open segment as the chat should be showing it: what it has said so far, the
    /// status line under that, and the figures closing an answer under both. The message
    /// holding it is sent once the turn has run long enough to be worth watching, and
    /// rewritten as the segment grows.
    pub(super) fn show(&mut self, id: &str) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(turn) = session.turn.as_ref() else {
            return;
        };
        let links = Links::new(self.app, id);
        if turn.due(links).is_none_or(|at| at > Instant::now()) {
            return;
        }
        let elapsed = turn.started.elapsed();
        let text = turn
            .segment
            .as_ref()
            .expect("a segment falling due")
            .text(true, links);
        let head = session.head(id, Some(&turn.prompt_id));
        let live = turn.live;
        let thread = turn.thread;
        let mut footer = status(elapsed, turn.seed);
        let (hit, spent) = (session.hit(), session.spent());
        if let Some(line) = status_line(session.context(), hit, spent, self.limits.as_ref()) {
            footer = format!("{footer}\n\n{line}");
        }
        let mut shown = hook::compose(&head, &took(elapsed), "", &running(&text, &footer));
        // A segment past a rich message reaches the chat as a file once it is complete,
        // and until then the live message keeps the last of it that fitted. A delta can
        // end anywhere in the markdown, so the text is never split at one.
        if !telegram::fits(&shown.markdown) {
            if live.is_some() {
                if let Some(segment) = self.turn_mut(id).and_then(|turn| turn.segment.as_mut()) {
                    segment.written = Some((text, Instant::now()));
                }
                return;
            }
            shown = hook::compose(&head, &took(elapsed), "", &footer);
        }
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

    /// `/stop` sends Escape to the session a message would reach while it runs a turn,
    /// once until the transcript says whether the turn stopped, since a second Escape on
    /// an idle prompt opens the `/rewind` picker.
    pub(super) fn stop(&mut self, from: &Admitted, replied: Option<&Message>) {
        let place = from.place;
        let address = match self.addressee(place, replied) {
            Ok(address) => address.filter(|address| address != NEW),
            Err(error) => return self.say(place, error),
        };
        let Some((id, session)) = address.and_then(|address| self.addressed(&address)) else {
            return self.say(place, NOTHING_RUNS);
        };
        let (id, dir) = (id.clone(), session.dir.clone());
        if !self.reaches(from.guest.as_deref(), place, &dir) {
            return;
        }
        let session = &self.sessions[&id];
        let Some(turn) = &session.turn else {
            return self.say(place, NOTHING_RUNS);
        };
        if turn.stopping.is_some() {
            return;
        }
        let Some(path) = session
            .transcript
            .as_ref()
            .map(|transcript| transcript.path.clone())
        else {
            self.say(place, "the session names no transcript to tell a stop by");
            return;
        };
        let pane = match self.terminal(&id) {
            Ok(pane) => pane,
            Err(error) => {
                self.say(place, &error);
                return;
            }
        };
        // A transcript not written yet holds nothing to leave out.
        let from = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        if let Err(error) = pane.interrupt() {
            self.say(place, &format!("tmux: {}", hook::prose(&error)));
            return;
        }
        if let Some(turn) = self.turn_mut(&id) {
            turn.stopping = Some(Stopping {
                from,
                until: Instant::now() + STOP_WAIT,
            });
        }
    }

    /// Ends a turn its transcript shows interrupted since Escape was sent, as `Stop`
    /// would, and says in its thread that it did not stop once `STOP_WAIT` has passed
    /// without that.
    pub(super) fn stopped(&mut self, id: &str) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let (Some(turn), Some(transcript)) = (session.turn.as_mut(), &session.transcript) else {
            return;
        };
        let Some(stopping) = &turn.stopping else {
            return;
        };
        if interrupted(&transcript.path, stopping.from) {
            self.interrupted(id);
            return;
        }
        if stopping.until > Instant::now() {
            return;
        }
        turn.stopping = None;
        let thread = turn.thread;
        let text = format!(
            "the turn did not stop within {}s of Escape",
            STOP_WAIT.as_secs()
        );
        self.telegram
            .send(thread.place, &text, Sound::Silent, thread.prompt);
    }

    /// What the open segment said stays, and the message showing it running goes. The
    /// press that asked for this is what the chat was waiting on, so it makes no sound.
    fn interrupted(&mut self, id: &str) {
        self.seal(id);
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(turn) = session.turn.take() else {
            return;
        };
        session.done = Some(turn.prompt_id.clone());
        if let Some(transcript) = &mut session.transcript {
            transcript.ended();
        }
        let head = session.head(id, Some(&turn.prompt_id));
        let message = hook::compose(
            &head,
            &took(turn.started.elapsed()),
            "#interrupted",
            "Interrupted",
        )
        .markdown;
        let thread = turn.thread;
        let posted = self
            .telegram
            .send(thread.place, &message, Sound::Silent, thread.prompt);
        session.left(thread.place, posted);
        if let Some(live) = turn.live {
            self.telegram.delete(thread.place.chat, live);
        }
    }
}

/// True once the transcript at `path` holds an interruption written past byte `from`.
fn interrupted(path: &Path, from: u64) -> bool {
    transcript::entries(path, from)
        .0
        .iter()
        .filter(|entry| entry["type"] == "user")
        .filter_map(|entry| entry["message"]["content"].as_array())
        .flatten()
        .any(|part| {
            part["text"]
                .as_str()
                .is_some_and(|text| text.starts_with(INTERRUPTED))
        })
}

/// The line linking to the changes since `since` of the working tree at `top`, which
/// `path` names in the session's directory, `None` when nothing changed.
pub(super) fn review(
    links: Links,
    top: &Path,
    since: &str,
    path: &str,
) -> Result<Option<String>, String> {
    let summary = changes::summary(top, since)?;
    if summary.files == 0 {
        return Ok(None);
    }
    let link = links
        .changes(since, path)
        .ok_or("the link would be longer than Telegram passes on")?;
    Ok(Some(format!(
        "[Review changes: {}]({link})",
        counted(&summary)
    )))
}

/// How a review link counts its changes, as `3 commits, 5 files`.
fn counted(&Summary { commits, files }: &Summary) -> String {
    let plural =
        |count: usize, noun: &str| format!("{count} {noun}{}", if count == 1 { "" } else { "s" });
    let files = plural(files, "file");
    if commits == 0 {
        files
    } else {
        format!("{}, {files}", plural(commits, "commit"))
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

/// The command `/<name>` the daemon typed into a session and Claude Code has yet to carry
/// out, which it never reports as a prompt.
fn typed<'a>(asked: &'a VecDeque<Ask>, name: &str) -> Option<&'a Ask> {
    asked.iter().find(|ask| {
        ask.text
            .strip_prefix('/')
            .and_then(|rest| rest.strip_prefix(name))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    })
}

/// Takes the command `/<name>` the daemon typed into a session once Claude Code carried
/// it out, along with everything typed before it.
pub(super) fn take_typed(asked: &mut VecDeque<Ask>, name: &str) -> Option<Thread> {
    let text = typed(asked, name)?.text.clone();
    pair(asked, &text)
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
        assert_eq!(segment.text(true, None), "first second third");
        // The run the words introduce stands under them.
        segment.calls.push(Call {
            id: "toolu_1".to_owned(),
            agent: None,
            name: "Read".to_owned(),
            description: String::new(),
            subject: "a".to_owned(),
            arguments: None,
            outcome: Outcome::Summed(Duration::from_secs(1), String::new()),
        });
        assert_eq!(
            segment.text(true, None),
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
            files: Vec::new(),
            place: HERE,
            message,
            guest: None,
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
    fn a_command_klaudo_typed_is_found_with_or_without_arguments() {
        let found = |texts: &[&str]| {
            let asked = VecDeque::from_iter(texts.iter().zip(1..).map(|(text, message)| Ask {
                text: (*text).to_owned(),
                files: Vec::new(),
                place: HERE,
                message,
                guest: None,
            }));
            typed(&asked, "compact").map(|ask| ask.message)
        };
        assert_eq!(found(&["/compactor", "/compact"]), Some(2));
        assert_eq!(found(&["/compact keep the plan"]), Some(1));
        assert_eq!(found(&["compact it"]), None);
    }
}

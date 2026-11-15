//! The transcript Claude Code writes of a session, the one record of an interruption, of
//! the order of what a turn said and called, of the context of a session that draws no status line
//! and of the tokens each call spent, and the conversation a rewound copy starts from.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use super::now_millis;
use super::price::Billed;
use super::usage::Usage;

pub(super) struct Transcript {
    pub(super) path: PathBuf,
    main: Tail,
    /// The transcripts of the session's subagents, which Claude Code writes in a
    /// directory beside it.
    subagents: HashMap<PathBuf, Tail>,
    /// The tokens the latest call of the main conversation read so far sent.
    tokens: Option<u64>,
    /// The cache hit of the turn's first call, once it has been read.
    hit: Option<f64>,
    /// Whether the next call read starts a turn.
    opening: bool,
    /// When the turn began to be counted, in Unix milliseconds.
    since: u64,
    /// What the calls read since the daemon heard of the session cost.
    spent: Spent,
    /// Whether the end of the turn has been read, which Claude Code writes once the Stop
    /// hooks have run, together with the turn's last call.
    closed: bool,
    /// What the main conversation recorded of the running turn and the caller has yet to
    /// take, in order.
    blocks: Vec<Block>,
}

/// A block of an assistant message of the main conversation. A message holds text,
/// or text or narration followed by its tool calls.
#[derive(Debug, PartialEq)]
pub(super) enum Block {
    Text(String),
    /// What the terminal shows above the tool calls of a message, which Claude Code
    /// records as a thinking block with text, while the model's own reasoning is written
    /// empty. No hook reports it.
    Narration(String),
    Call {
        id: String,
        name: String,
        input: Map<String, Value>,
    },
}

/// How far a transcript has been read, and the call it last counted the cost of.
#[derive(Default)]
struct Tail {
    read: u64,
    call: Option<String>,
}

#[derive(Clone, Copy)]
enum Spent {
    Nothing,
    Dollars(f64),
    /// A call ran on a model with no known price.
    Unpriced,
}

impl Transcript {
    /// Read from where it and the transcripts of its subagents end when first heard of.
    pub(super) fn new(path: PathBuf) -> Self {
        let subagents = subagents(&path)
            .map(|subagent| {
                let read = length(&subagent);
                (subagent, Tail { read, call: None })
            })
            .collect();
        Self {
            main: Tail {
                read: length(&path),
                call: None,
            },
            path,
            subagents,
            tokens: None,
            hit: None,
            opening: true,
            since: now_millis(),
            spent: Spent::Nothing,
            closed: false,
            blocks: Vec::new(),
        }
    }

    /// The tokens the latest call of the main conversation sent, which is how full its
    /// context is, as the status line counts it.
    pub(super) fn tokens(&mut self) -> Option<u64> {
        self.read();
        self.tokens
    }

    /// The percentage of what the turn's first call sent that was read from the cache.
    pub(super) fn hit(&mut self) -> Option<f64> {
        self.read();
        self.hit
    }

    /// What the calls made since the daemon heard of the session, its subagents' included,
    /// cost at list prices, once one has been read and every one ran on a model with a
    /// known price.
    pub(super) fn spent(&mut self) -> Option<f64> {
        self.read();
        match self.spent {
            Spent::Dollars(dollars) => Some(dollars),
            Spent::Nothing | Spent::Unpriced => None,
        }
    }

    /// Whether the transcript records the end of the turn, and with it every call.
    pub(super) fn closed(&mut self) -> bool {
        self.read();
        self.closed
    }

    /// The blocks recorded since the last call, in order.
    pub(super) fn recorded(&mut self) -> Vec<Block> {
        self.read();
        std::mem::take(&mut self.blocks)
    }

    /// The turn has ended, so the next call starts another.
    pub(super) fn ended(&mut self) {
        self.read();
        self.hit = None;
        self.opening = true;
        self.since = now_millis();
        self.closed = false;
        self.blocks.clear();
    }

    fn read(&mut self) {
        let (appended, read) = entries(&self.path, self.main.read);
        self.main.read = read;
        for usage in appended.iter().filter_map(sent) {
            if self.opening {
                self.hit = Some(usage.hit());
                self.opening = false;
            }
            self.tokens = Some(usage.total());
        }
        self.spent = charge(self.spent, &mut self.main.call, &appended, self.since);
        self.closed |= appended
            .iter()
            .any(|entry| entry["subtype"] == "stop_hook_summary" && !before(entry, self.since));
        for entry in &appended {
            self.record(entry);
        }
        for subagent in subagents(&self.path) {
            let tail = self.subagents.entry(subagent.clone()).or_default();
            let (appended, read) = entries(&subagent, tail.read);
            tail.read = read;
            self.spent = charge(self.spent, &mut tail.call, &appended, self.since);
        }
    }

    /// Claude Code writes each block of an assistant message as an entry of its own, and
    /// the end of a turn after the turn's last message, so what was recorded ahead of
    /// that end belongs to a turn that is over.
    fn record(&mut self, entry: &Value) {
        if entry["subtype"] == "stop_hook_summary" {
            self.blocks.clear();
            return;
        }
        if entry["type"] != "assistant" || entry["isSidechain"] == true {
            return;
        }
        for block in entry["message"]["content"].as_array().into_iter().flatten() {
            let field = |key: &str| block[key].as_str().unwrap_or_default();
            let recorded = match block["type"].as_str() {
                Some("text") if !field("text").trim().is_empty() => {
                    Block::Text(field("text").to_owned())
                }
                Some("thinking") if !field("thinking").trim().is_empty() => {
                    Block::Narration(field("thinking").trim().to_owned())
                }
                Some("tool_use") => Block::Call {
                    id: field("id").to_owned(),
                    name: field("name").to_owned(),
                    input: block["input"].as_object().cloned().unwrap_or_default(),
                },
                _ => continue,
            };
            self.blocks.push(recorded);
        }
    }
}

/// The transcripts of the subagents of the session whose transcript is at `path`.
fn subagents(path: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(path.with_extension("").join("subagents"))
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
}

fn length(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

/// Adds to `spent` what the calls in `entries` cost, each once though Claude Code writes
/// an entry for each block of its answer, and none made before `since`, which Claude Code
/// writes again when it resumes a session.
fn charge(mut spent: Spent, call: &mut Option<String>, entries: &[Value], since: u64) -> Spent {
    for entry in entries {
        let message = &entry["message"];
        let Some(model) = message["model"].as_str() else {
            continue;
        };
        if entry["type"] != "assistant"
            || entry["isUnmetered"] == true
            || model == "<synthetic>"
            || before(entry, since)
        {
            continue;
        }
        let id = message["id"].as_str();
        if id.is_some() && id == call.as_deref() {
            continue;
        }
        *call = id.map(str::to_owned);
        let Ok(billed) = Billed::deserialize(&message["usage"]) else {
            continue;
        };
        spent = match (spent, billed.cost(model)) {
            (Spent::Unpriced, _) => Spent::Unpriced,
            (_, None) => {
                eprintln!("no price for {model}");
                Spent::Unpriced
            }
            (Spent::Nothing, Some(cost)) => Spent::Dollars(cost),
            (Spent::Dollars(dollars), Some(cost)) => Spent::Dollars(dollars + cost),
        };
    }
    spent
}

/// Whether `entry` was written before `since`, in Unix milliseconds.
fn before(entry: &Value, since: u64) -> bool {
    entry["timestamp"]
        .as_str()
        .and_then(unix_millis)
        .is_some_and(|at| at < since)
}

/// The Unix milliseconds of a timestamp as Claude Code writes it, `2026-10-23T10:08:53.339Z`.
fn unix_millis(timestamp: &str) -> Option<u64> {
    let number = |at: usize, digits: usize| timestamp.get(at..at + digits)?.parse::<u64>().ok();
    let (year, month, day) = (number(0, 4)?, number(5, 2)?, number(8, 2)?);
    // Days since 1970 by the proleptic Gregorian calendar, in years that start in March
    // so that a leap day ends them.
    let year = if month <= 2 { year - 1 } else { year };
    let (era, of_era) = (year / 400, year % 400);
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + of_era * 365 + of_era / 4 - of_era / 100 + of_year - 719_468;
    let seconds = ((days * 24 + number(11, 2)?) * 60 + number(14, 2)?) * 60 + number(17, 2)?;
    Some(seconds * 1000 + number(20, 3)?)
}

/// The entries written to the transcript at `path` past byte `from`, and the byte after
/// the last of them. A line still being written is left for the next read.
pub(super) fn entries(path: &Path, from: u64) -> (Vec<serde_json::Value>, u64) {
    let mut appended = Vec::new();
    let read = File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(from))?;
        file.read_to_end(&mut appended)
    });
    if let Err(error) = read {
        eprintln!("{}: {error}", path.display());
        return (Vec::new(), from);
    }
    let end = appended
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |at| at + 1);
    let entries = String::from_utf8_lossy(&appended[..end])
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    (entries, from + end as u64)
}

/// A conversation copied up to the end of the turn it was rewound to: the copy's session,
/// and the prompt of the first turn it left out.
pub(super) struct Rewound {
    pub(super) id: String,
    pub(super) prompt: String,
}

/// Copies the conversation in the transcript at `path` up to the end of a turn into the
/// transcript of a new session beside it. The turn is the one whose prompt id starts with
/// `turn`, or else the latest compaction. Nothing is copied when there is no such turn or
/// nothing follows it. Claude Code resumes a session from the file named after its id in
/// the directory of its project, and every entry names its session.
pub(super) fn rewind(path: &Path, turn: Option<&str>) -> io::Result<Option<Rewound>> {
    let text = fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let entries: Vec<Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).unwrap_or_default())
        .collect();
    let prompts = || {
        entries.iter().enumerate().filter_map(|(at, entry)| {
            let prompt = entry["promptId"].as_str()?;
            (entry["type"] == "user").then_some((at, prompt))
        })
    };
    // The summary of a compaction and what Claude Code adds with it carry the prompt of
    // the compaction.
    let kept = match turn {
        Some(turn) => prompts().find(|(_, prompt)| prompt.starts_with(turn)),
        None => entries
            .iter()
            .rposition(|entry| entry["type"] == "system" && entry["subtype"] == "compact_boundary")
            .and_then(|boundary| prompts().find(|&(at, _)| at > boundary)),
    };
    let Some((start, kept)) = kept else {
        return Ok(None);
    };
    let Some((next, _)) = prompts().find(|&(at, prompt)| at > start && prompt != kept) else {
        return Ok(None);
    };
    // What Claude Code writes ahead of a prompt belongs to its turn.
    let end = entries[..next]
        .iter()
        .rposition(|entry| entry.get("uuid").is_some())
        .map_or(0, |last| last + 1);
    let id = fs::read_to_string("/proc/sys/kernel/random/uuid")?
        .trim()
        .to_owned();
    let prompt = said(&entries[next]);
    let mut copy = String::new();
    for (line, entry) in lines.iter().zip(entries).take(end) {
        match entry {
            Value::Object(mut entry) => {
                for key in ["sessionId", "session_id"] {
                    if let Some(session) = entry.get_mut(key) {
                        *session = Value::from(id.as_str());
                    }
                }
                copy.push_str(&Value::Object(entry).to_string());
            }
            _ => copy.push_str(line),
        }
        copy.push('\n');
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path.with_file_name(format!("{id}.jsonl")))?
        .write_all(copy.as_bytes())?;
    Ok(Some(Rewound { id, prompt }))
}

/// The text of a user entry, which is a string or a list of blocks.
fn said(entry: &Value) -> String {
    let content = &entry["message"]["content"];
    match content.as_array() {
        Some(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        None => content.as_str().unwrap_or_default().to_owned(),
    }
}

/// The tokens an assistant entry of the main conversation sent, for an entry the API
/// answered. Claude Code writes the rest itself, with no tokens or none it counts.
fn sent(entry: &serde_json::Value) -> Option<Usage> {
    if entry["type"] != "assistant"
        || entry["isSidechain"] == true
        || entry["isUnmetered"] == true
        || entry["message"]["model"] == "<synthetic>"
    {
        return None;
    }
    let usage = &entry["message"]["usage"];
    // A call the API ran in several iterations holds the context of its last message.
    let usage = usage["iterations"]
        .as_array()
        .and_then(|iterations| {
            iterations
                .iter()
                .rfind(|it| it["type"] != "advisor_message" && it["type"] != "compaction")
        })
        .filter(|last| last["type"] == "message")
        .unwrap_or(usage);
    Usage::deserialize(usage)
        .ok()
        .filter(|usage| usage.total() > 0)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::*;

    fn said(usage: &serde_json::Value) -> serde_json::Value {
        json!({"type": "assistant", "message": {"model": "claude-opus-5-5", "usage": usage}})
    }

    fn total(entry: &serde_json::Value) -> Option<u64> {
        sent(entry).map(|usage| usage.total())
    }

    fn usage(input: u64, written: u64, cached: u64) -> serde_json::Value {
        json!({"input_tokens": input, "cache_creation_input_tokens": written,
            "cache_read_input_tokens": cached, "output_tokens": 459})
    }

    #[test]
    fn a_call_sent_its_input_with_what_it_wrote_and_read_of_the_cache() {
        assert_eq!(total(&said(&usage(2, 403, 112_257))), Some(112_662));
    }

    #[test]
    fn a_call_in_iterations_sent_what_its_last_message_did() {
        let mut iterated = usage(30, 30, 30);
        iterated["iterations"] = json!([
            {"type": "message", "input_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            {"type": "message", "input_tokens": 1, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3},
            {"type": "compaction", "input_tokens": 9, "cache_creation_input_tokens": 9, "cache_read_input_tokens": 9},
        ]);
        assert_eq!(total(&said(&iterated)), Some(6));
    }

    #[test]
    fn what_claude_code_writes_itself_sent_nothing() {
        let mut synthetic = said(&usage(0, 0, 0));
        synthetic["message"]["model"] = json!("<synthetic>");
        let mut subagent = said(&usage(1, 1, 1));
        subagent["isSidechain"] = json!(true);
        let mut unmetered = said(&usage(1, 1, 1));
        unmetered["isUnmetered"] = json!(true);
        let user = json!({"type": "user", "message": {"content": "hi"}});
        for entry in [synthetic, subagent, unmetered, user, said(&usage(0, 0, 0))] {
            assert_eq!(total(&entry), None, "{entry}");
        }
    }

    #[test]
    fn the_latest_call_counts_once_its_line_is_whole() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("transcript.jsonl");
        let mut file = File::create(&path).expect("a transcript");
        writeln!(file, "{}", said(&usage(1, 0, 0))).expect("a line");
        let mut transcript = Transcript::new(path);
        assert_eq!(transcript.tokens(), None);

        let line = said(&usage(2, 0, 0)).to_string();
        let (head, tail) = line.split_at(line.len() / 2);
        write!(file, "{}\n{head}", said(&usage(3, 0, 0))).expect("a line and a half");
        assert_eq!(transcript.tokens(), Some(3));
        writeln!(file, "{tail}").expect("the rest of the line");
        writeln!(
            file,
            "{}",
            json!({"type": "user", "message": {"content": "next"}})
        )
        .expect("a prompt");
        assert_eq!(transcript.tokens(), Some(2));
        assert_eq!(transcript.tokens(), Some(2));
    }

    #[test]
    fn a_turn_s_blocks_are_taken_in_order_until_it_ends() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("transcript.jsonl");
        let mut file = File::create(&path).expect("a transcript");
        let mut transcript = Transcript::new(path);
        let block =
            |content: Value| json!({"type": "assistant", "message": {"content": [content]}});
        let text = |text: &str| block(json!({"type": "text", "text": text}));
        let thinking = |text: &str| block(json!({"type": "thinking", "thinking": text}));
        let call = |id: &str| {
            block(
                json!({"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": "/a"}}),
            )
        };
        let read = |id: &str| Block::Call {
            id: id.to_owned(),
            name: "Read".to_owned(),
            input: json!({"file_path": "/a"})
                .as_object()
                .cloned()
                .expect("an object"),
        };
        let mut subagent = call("t0");
        subagent["isSidechain"] = json!(true);
        for entry in [
            subagent,
            text("on it\n"),
            thinking(" a\n\n"),
            thinking(""),
            call("t1"),
        ] {
            writeln!(file, "{entry}").expect("a line");
        }
        assert_eq!(
            transcript.recorded(),
            [
                Block::Text("on it\n".to_owned()),
                Block::Narration("a".to_owned()),
                read("t1")
            ]
        );
        assert_eq!(transcript.recorded(), [], "a block is taken once");

        // The last message of a turn is written ahead of the turn's end, once the turn
        // is over.
        let end = json!({"type": "system", "subtype": "stop_hook_summary"});
        writeln!(file, "{}\n{end}\n{}", text("done"), call("t2")).expect("lines");
        assert_eq!(transcript.recorded(), [read("t2")]);

        writeln!(file, "{}", thinking("b")).expect("a line");
        transcript.ended();
        assert_eq!(transcript.recorded(), [], "a turn's blocks end with it");
    }

    /// The entries of the copy rewinding `lines` to `turn` makes, by their uuids, and the
    /// prompt of the turn.
    fn rewound(
        lines: &[serde_json::Value],
        turn: Option<&str>,
    ) -> Option<(Vec<serde_json::Value>, String)> {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("s.jsonl");
        let text: String = lines.iter().map(|line| line.to_string() + "\n").collect();
        fs::write(&path, text).expect("a transcript");
        let rewound = rewind(&path, turn).expect("a rewind")?;
        let copy = directory.path().join(format!("{}.jsonl", rewound.id));
        let mode = fs::metadata(&copy).expect("the copy").permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        let entries = fs::read_to_string(copy)
            .expect("the copy")
            .lines()
            .map(|line| serde_json::from_str(line).expect("an entry"))
            .collect();
        Some((entries, rewound.prompt))
    }

    fn uuids(copy: &[serde_json::Value]) -> Vec<&str> {
        copy.iter()
            .map(|entry| entry["uuid"].as_str().unwrap_or_default())
            .collect()
    }

    fn prompt(uuid: &str, prompt: &str) -> serde_json::Value {
        json!({"type": "user", "uuid": uuid, "promptId": prompt, "sessionId": "s",
            "message": {"content": uuid}})
    }

    /// A conversation compacted twice, the second time by `/compact`, whose prompt its
    /// summary and what Claude Code adds with it carry.
    fn compacted() -> Vec<serde_json::Value> {
        let mut summary = prompt("summary", "c2");
        summary["session_id"] = json!("s");
        vec![
            prompt("first", "p1"),
            json!({"type": "system", "subtype": "compact_boundary", "uuid": "b1", "sessionId": "s"}),
            prompt("between", "p2"),
            json!({"type": "assistant", "uuid": "answer", "sessionId": "s"}),
            prompt("compact", "c2"),
            json!({"type": "system", "subtype": "compact_boundary", "uuid": "b2", "sessionId": "s"}),
            summary,
            prompt("echo", "c2"),
            json!({"type": "attachment", "uuid": "file", "sessionId": "s"}),
            json!({"type": "last-prompt", "sessionId": "s", "lastPrompt": "after"}),
            prompt("after", "p3"),
            json!({"type": "user", "uuid": "blocks", "promptId": "p4", "sessionId": "s",
                "message": {"content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]}}),
        ]
    }

    const AFTER_COMPACTION: [&str; 9] = [
        "first", "b1", "between", "answer", "compact", "b2", "summary", "echo", "file",
    ];

    #[test]
    fn a_rewind_keeps_the_latest_compaction_up_to_the_next_prompt() {
        let (copy, prompt) = rewound(&compacted(), None).expect("a copy");
        assert_eq!(uuids(&copy), AFTER_COMPACTION);
        assert_eq!(prompt, "after");
        let id = copy[0]["sessionId"].as_str().expect("a session");
        assert_ne!(id, "s");
        assert!(copy.iter().all(|entry| entry["sessionId"] == id));
        assert_eq!(copy[6]["session_id"], id);
    }

    #[test]
    fn a_rewind_to_a_turn_keeps_it_up_to_the_next_prompt() {
        let (copy, prompt) = rewound(&compacted(), Some("p2")).expect("a copy");
        assert_eq!(uuids(&copy), ["first", "b1", "between", "answer"]);
        assert_eq!(prompt, "compact");
        let (copy, prompt) = rewound(&compacted(), Some("c2")).expect("a copy");
        assert_eq!(uuids(&copy), AFTER_COMPACTION);
        assert_eq!(prompt, "after");
        let (copy, prompt) = rewound(&compacted(), Some("p3")).expect("a copy");
        assert_eq!(uuids(&copy).last(), Some(&"after"));
        assert_eq!(prompt, "one\ntwo");
    }

    #[test]
    fn a_rewind_with_nothing_to_leave_out_copies_nothing() {
        let boundary = json!({"type": "system", "subtype": "compact_boundary", "uuid": "b"});
        assert!(rewound(&[prompt("first", "p1")], None).is_none());
        assert!(
            rewound(
                &[prompt("first", "p1"), boundary, prompt("summary", "c")],
                None
            )
            .is_none()
        );
        assert!(rewound(&compacted(), Some("p4")).is_none());
        assert!(rewound(&compacted(), Some("p9")).is_none());
    }

    #[test]
    fn a_turn_shows_what_its_first_call_read_from_the_cache() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("transcript.jsonl");
        let mut file = File::create(&path).expect("a transcript");
        let mut transcript = Transcript::new(path);
        writeln!(file, "{}", said(&usage(10, 10, 80))).expect("the first call");
        writeln!(file, "{}", said(&usage(0, 0, 100))).expect("the second call");
        assert_eq!(transcript.hit(), Some(80.0));

        transcript.ended();
        assert_eq!(transcript.hit(), None);
        writeln!(file, "{}", said(&usage(25, 75, 0))).expect("an expired cache");
        writeln!(file, "{}", said(&usage(1, 0, 99))).expect("the second call");
        assert_eq!(transcript.hit(), Some(0.0));
        assert_eq!(transcript.tokens(), Some(100));
    }

    /// A call of `id` that cost $4 on Claude Opus 5.5, as one entry of its answer written
    /// at `timestamp`.
    fn billed(id: &str, timestamp: &str) -> serde_json::Value {
        let mut entry = said(&usage(1_000_000, 0, 0));
        entry["message"]["usage"]["output_tokens"] = json!(0);
        entry["message"]["id"] = json!(id);
        entry["timestamp"] = json!(timestamp);
        entry
    }

    const NOW: &str = "2999-01-01T00:00:00.000Z";

    fn cents(transcript: &mut Transcript) -> Option<f64> {
        transcript.spent().map(|dollars| (dollars * 100.0).round())
    }

    #[test]
    fn a_session_costs_each_of_its_calls_once() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("s.jsonl");
        let mut file = File::create(&path).expect("a transcript");
        writeln!(file, "{}", billed("old", NOW)).expect("a call before");
        let mut transcript = Transcript::new(path);
        assert_eq!(transcript.spent(), None);

        for line in [
            billed("m1", NOW),
            billed("m1", NOW),
            billed("m2", NOW),
            // What Claude Code writes again when it resumes the session.
            billed("m1", "2000-01-01T00:00:00.000Z"),
        ] {
            writeln!(file, "{line}").expect("a call");
        }
        assert_eq!(cents(&mut transcript), Some(800.0));

        transcript.ended();
        writeln!(file, "{}", billed("m3", NOW)).expect("a call");
        assert_eq!(cents(&mut transcript), Some(1200.0));
        let mut unknown = billed("m4", NOW);
        unknown["message"]["model"] = json!("claude-opus-9");
        writeln!(file, "{unknown}\n{}", billed("m5", NOW)).expect("calls");
        assert_eq!(transcript.spent(), None);
        transcript.ended();
        writeln!(file, "{}", billed("m6", NOW)).expect("a call");
        assert_eq!(transcript.spent(), None);
    }

    #[test]
    fn a_session_costs_what_its_subagents_called_since_it_was_heard_of() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("s.jsonl");
        File::create(&path).expect("a transcript");
        let subagents = directory.path().join("s").join("subagents");
        fs::create_dir_all(&subagents).expect("a directory of subagents");
        let mut earlier = File::create(subagents.join("agent-a.jsonl")).expect("a subagent");
        writeln!(earlier, "{}", billed("a1", NOW)).expect("a call before");
        let mut transcript = Transcript::new(path);

        writeln!(earlier, "{}", billed("a2", NOW)).expect("a call");
        let mut later = File::create(subagents.join("agent-b.jsonl")).expect("a subagent");
        writeln!(later, "{}", billed("b1", NOW)).expect("a call");
        File::create(subagents.join("agent-b.meta.json")).expect("its metadata");
        assert_eq!(cents(&mut transcript), Some(800.0));
    }

    #[test]
    fn a_timestamp_reads_as_unix_milliseconds() {
        assert_eq!(unix_millis("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            unix_millis("2000-02-29T23:59:59.999Z"),
            Some(951_868_799_999)
        );
        assert_eq!(
            unix_millis("2026-10-23T10:08:53.339Z"),
            Some(1_792_750_133_339)
        );
        assert_eq!(unix_millis("yesterday"), None);
    }
}

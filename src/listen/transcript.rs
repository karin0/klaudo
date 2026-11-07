//! The transcript Claude Code writes of a session, the one record of an interruption and
//! of the context of a session that draws no status line.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::usage::Usage;

pub(super) struct Transcript {
    pub(super) path: PathBuf,
    /// How far it has been read for the context.
    read: u64,
    /// The tokens the latest call of the main conversation read so far sent.
    tokens: Option<u64>,
    /// The cache hit of the turn's first call, once it has been read.
    hit: Option<f64>,
    /// Whether the next call read starts a turn.
    opening: bool,
}

impl Transcript {
    /// Read from where it ends when first heard of.
    pub(super) fn new(path: PathBuf) -> Self {
        let read = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        Self {
            path,
            read,
            tokens: None,
            hit: None,
            opening: true,
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

    /// The turn has ended, so the next call starts another.
    pub(super) fn ended(&mut self) {
        self.read();
        self.hit = None;
        self.opening = true;
    }

    fn read(&mut self) {
        let (entries, read) = entries(&self.path, self.read);
        self.read = read;
        for usage in entries.iter().filter_map(sent) {
            if self.opening {
                self.hit = Some(usage.hit());
                self.opening = false;
            }
            self.tokens = Some(usage.total());
        }
    }
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
}

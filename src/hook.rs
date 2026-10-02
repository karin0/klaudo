use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::telegram::{ELLIPSIS, MAX_BYTES, Post};

/// Fields that identify the invocation rather than describe it, dropped from the
/// verbatim report an unrecognised event falls back to.
const BOILERPLATE: [&str; 2] = ["permission_mode", "effort"];

/// The field a tool's input says what the call is doing in, where the tool carries one.
const DESCRIPTION: [&str; 1] = ["description"];
/// The field of a tool's input that names what the call is working on, tried in this
/// order because nothing in the event marks which field that is.
const SUBJECT: [&str; 7] = [
    "command",
    "file_path",
    "pattern",
    "url",
    "query",
    "path",
    "prompt",
];
/// How far a field of a tool's input reads, past which the rest says nothing at a
/// glance. Three lines of a phone's screen, which a command fills before a reader has
/// stopped taking it in.
const SUBJECT_MAX: usize = 120;

/// How many bytes of the reasoning a compaction's model writes ahead of its summary the
/// message carries, since that reasoning is the part of the message a reader skips.
const ANALYSIS_MAX: usize = 4000;

/// Marks the end of a turn, so a chat holding several projects can be filtered down to
/// the replies that finished a piece of work.
const TAG: &str = "#claude";

/// One hook invocation as Claude Code writes it on stdin. Fields no arm below reads
/// survive in `rest`, so an event this program has never seen still reports what it
/// carries instead of arriving empty.
#[derive(Deserialize)]
pub struct Event {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_input: Map<String, Value>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub delta: Option<String>,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    /// What started a compaction: `manual` for `/compact`, `auto` for a full context.
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub compact_summary: Option<String>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

impl Event {
    /// The directory the session belongs to. A `cd` in a turn moves `cwd` for every
    /// event after it, while Claude Code keeps filing the transcript under the
    /// directory the session was opened in, so that is the ancestor to report.
    pub fn directory(&self) -> Option<PathBuf> {
        let filed = Path::new(self.transcript_path.as_deref()?)
            .parent()?
            .file_name()?
            .to_str()?;
        Path::new(&self.cwd)
            .ancestors()
            .find(|dir| slug(dir) == filed)
            .map(Path::to_path_buf)
    }

    /// What a tool call says it is doing, in one line, empty where its tool describes
    /// no call of itself.
    pub fn description(&self) -> String {
        self.field(&DESCRIPTION)
    }

    /// What a tool call is working on, in one line.
    pub fn subject(&self) -> String {
        self.field(&SUBJECT)
    }

    fn field(&self, keys: &[&str]) -> String {
        let Some(value) = keys
            .iter()
            .find_map(|key| self.tool_input.get(*key)?.as_str())
        else {
            return String::new();
        };
        let flat = value.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.chars().count() <= SUBJECT_MAX {
            return flat;
        }
        flat.chars()
            .take(SUBJECT_MAX)
            .chain("\u{2026}".chars())
            .collect()
    }

    fn tag(&self) -> String {
        match self.hook_event_name.as_str() {
            // The turn opens with what was asked; the tag belongs to what closes it.
            "UserPromptSubmit" => String::new(),
            "Stop" => TAG.to_owned(),
            "StopFailure" => format!("{TAG} #failed"),
            "Notification" => format!("{TAG} #input"),
            // A compaction asked for is the whole of what that request did, while one
            // Claude Code started happens inside a turn whose `Stop` is still to come.
            "PostCompact" if self.manual() => format!("{TAG} #compact"),
            "PostCompact" => "#compact".to_owned(),
            other => format!("{TAG} #{other}"),
        }
    }

    /// What the message says under its title, in at most `room` bytes where the
    /// event carries more than a message holds.
    fn body(&self, room: usize) -> String {
        match self.hook_event_name.as_str() {
            // What was asked is quoted, so a chat scrolled through tells the asks from
            // the answers at a glance.
            "UserPromptSubmit" => quote(self.prompt.as_deref().unwrap_or_default()),
            // What Claude Code answered is markdown, and reads as the markdown it is.
            "Stop" => self.last_assistant_message.clone().unwrap_or_default(),
            "StopFailure" => prose(&self.error.clone().unwrap_or_else(|| self.residue())),
            "Notification" => prose(&self.message.clone().unwrap_or_else(|| self.residue())),
            "PostCompact" => compaction(self.compact_summary.as_deref().unwrap_or_default(), room),
            _ => prose(&self.residue()),
        }
    }

    pub fn manual(&self) -> bool {
        self.trigger.as_deref() == Some("manual")
    }

    fn residue(&self) -> String {
        let kept: Map<String, Value> = self
            .rest
            .iter()
            .filter(|(key, _)| !BOILERPLATE.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        serde_json::to_string(&kept).expect("a parsed object re-serializes")
    }
}

/// The name Claude Code files a project's transcripts under: every character outside
/// `[a-zA-Z0-9]` written as a dash.
fn slug(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// The characters this markdown gives a meaning to, which prose carrying one escapes.
/// Telegram consumes the backslash in front of exactly these and leaves one in front of
/// anything else standing in the text a client copies out.
const ESCAPED: &str = "\\_*[]()~`>#+-=|{}.!$";

/// What someone wrote, reaching the chat as they wrote it. A tag is read out of the
/// characters HTML owns, which carry no backslash escape and travel as entities.
pub fn prose(text: &str) -> String {
    let mut written = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => written.push_str("&amp;"),
            '<' => written.push_str("&lt;"),
            character => {
                if ESCAPED.contains(character) {
                    written.push('\\');
                }
                written.push(character);
            }
        }
    }
    written
}

/// The characters Telegram's HTML reserves.
pub fn html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A compaction's summary and the reasoning its model wrote ahead of it, each folded into
/// a quotation that opens on a tap. The summary is what Claude Code keeps, so it comes
/// first and takes the room it needs, and the reasoning gets what is left of the rest.
fn compaction(raw: &str, room: usize) -> String {
    let within = |tag: &str| {
        let (_, rest) = raw.split_once(&format!("<{tag}>"))?;
        let (inner, _) = rest.split_once(&format!("</{tag}>"))?;
        Some(inner.trim())
    };
    let summary = fold(within("summary").unwrap_or(raw.trim()), "summary", room);
    let left = room.saturating_sub(summary.len() + 2);
    match within("analysis").map(|analysis| fold(analysis, "analysis", left.min(ANALYSIS_MAX))) {
        Some(analysis) if !analysis.is_empty() => format!("{summary}\n\n{analysis}"),
        _ => summary,
    }
}

/// Text in a quotation that opens on a tap, credited with what it is, cut to `room`
/// bytes of markup. Markdown is not parsed inside a block HTML tag, so the text
/// travels as HTML. Empty when not even the markup fits.
fn fold(text: &str, credit: &str, room: usize) -> String {
    let open = "<blockquote expandable>";
    let close = format!("<cite>{credit}</cite></blockquote>");
    let Some(mut left) = room.checked_sub(open.len() + close.len() + ELLIPSIS.len_utf8()) else {
        return String::new();
    };
    let mut folded = String::from(open);
    for character in text.chars() {
        let escaped = match character {
            '\n' => "<br>".to_owned(),
            other => html(&other.to_string()),
        };
        let Some(rest) = left.checked_sub(escaped.len()) else {
            folded.push(ELLIPSIS);
            break;
        };
        left = rest;
        folded.push_str(&escaped);
    }
    folded.push_str(&close);
    folded
}

/// A block quotation, which every line carries its own marker of because a line without
/// one ends the quote.
fn quote(text: &str) -> String {
    text.lines()
        .map(|line| format!(">{}", prose(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn name(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or(dir.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// The line every message opens with: where the work is, then its address.
pub struct Head {
    project: String,
    address: String,
}

impl Head {
    pub fn new(dir: &Path, session: &str, prompt: Option<&str>) -> Self {
        Self {
            project: name(dir),
            address: address(session, prompt),
        }
    }

    pub fn markdown(&self) -> String {
        format!("**{}** `{}`", prose(&self.project), self.address)
    }

    pub fn html(&self) -> String {
        format!(
            "<b>{}</b> <code>{}</code>",
            html(&self.project),
            self.address
        )
    }
}

/// Which session, and which turn of that session. The prompt id is what tells one turn
/// from the next.
pub fn address(session: &str, prompt: Option<&str>) -> String {
    let short = |id: &str| id.chars().take(8).collect::<String>();
    match prompt {
        Some(prompt) => format!("{}/{}", short(session), short(prompt)),
        None => short(session),
    }
}

/// An untagged title is what a message in the middle of a turn carries.
pub fn compose(head: &Head, took: &str, tag: &str, body: &str) -> Post {
    let title = |head: String| format!("{head}{took}  {tag}").trim_end().to_owned();
    Post {
        markdown: format!("{}\n\n{body}", title(head.markdown())),
        caption: title(head.html()),
    }
}

pub fn message(event: &Event, head: &Head, took: &str) -> Post {
    let tag = event.tag();
    let title = compose(head, took, &tag, "").markdown.len();
    compose(head, took, &tag, &event.body(MAX_BYTES - title))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Head {
        Head::new(Path::new("/w/p"), "s", None)
    }

    fn event(json: serde_json::Value) -> Event {
        serde_json::from_value(json).expect("fixture")
    }

    #[test]
    fn a_project_is_the_last_segment_of_its_directory() {
        assert_eq!(name(Path::new("/home/user/scratch")), "scratch");
        assert_eq!(name(Path::new("/")), "/");
    }

    #[test]
    fn a_session_belongs_where_its_transcript_is_filed_rather_than_where_it_cd_ed() {
        let filed = |cwd: &str| {
            event(serde_json::json!({
                "hook_event_name": "Stop",
                "session_id": "s",
                "cwd": cwd,
                "transcript_path": "/home/u/.claude/projects/-home-u-dev-my-tree/s.jsonl",
            }))
            .directory()
        };
        assert_eq!(
            filed("/home/u/dev/my-tree"),
            Some("/home/u/dev/my-tree".into())
        );
        assert_eq!(
            filed("/home/u/dev/my-tree/vendor/lib"),
            Some("/home/u/dev/my-tree".into())
        );
        // A turn that walked out of the tree names no ancestor that was filed.
        assert_eq!(filed("/tmp"), None);
    }

    #[test]
    fn a_transcript_without_a_project_directory_places_nothing() {
        let bare = event(serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "s",
            "cwd": "/home/u/dev/my-tree",
        }));
        assert_eq!(bare.directory(), None);
    }

    #[test]
    fn a_call_reads_by_the_fields_of_its_input_that_say_what_it_does() {
        let call = |input: serde_json::Value| {
            let event = event(serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": "s",
                "tool_input": input,
            }));
            (event.description(), event.subject())
        };
        assert_eq!(
            call(serde_json::json!({"command": "cargo test", "description": "run the tests"})),
            ("run the tests".to_owned(), "cargo test".to_owned())
        );
        assert_eq!(
            call(serde_json::json!({"file_path": "/src/hook.rs"})),
            (String::new(), "/src/hook.rs".to_owned())
        );
        assert_eq!(
            call(serde_json::json!({"description": "find the seal", "prompt": "a paragraph"})),
            ("find the seal".to_owned(), "a paragraph".to_owned())
        );
        // A prompt is one line by the time it is a call's subject.
        assert_eq!(
            call(serde_json::json!({"prompt": "first\n  second"})).1,
            "first second"
        );
        assert_eq!(
            call(serde_json::json!({"todos": []})),
            (String::new(), String::new())
        );
        let long = call(serde_json::json!({"command": "x".repeat(SUBJECT_MAX + 5)})).1;
        assert_eq!(long.chars().count(), SUBJECT_MAX + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn a_head_addresses_the_turn_and_falls_back_to_the_session() {
        let dir = Path::new("/w/p");
        let turn = Head::new(dir, "0123456789abcdef", Some("fedcba9876543210"));
        assert_eq!(turn.markdown(), "**p** `01234567/fedcba98`");
        assert_eq!(turn.html(), "<b>p</b> <code>01234567/fedcba98</code>");
        assert_eq!(
            Head::new(dir, "0123456789abcdef", None).markdown(),
            "**p** `01234567`"
        );
        let odd = Head::new(Path::new("/w/a_<b>"), "s", None);
        assert_eq!(odd.markdown(), "**a\\_&lt;b\\>** `s`");
        assert_eq!(odd.html(), "<b>a_&lt;b&gt;</b> <code>s</code>");
    }

    #[test]
    fn a_title_without_a_tag_ends_at_the_elapsed_time() {
        let post = compose(&p(), " 12s", "", "text");
        assert_eq!(post.markdown, "**p** `s` 12s\n\ntext");
        assert_eq!(post.caption, "<b>p</b> <code>s</code> 12s");
    }

    #[test]
    fn stop_reports_the_last_assistant_message() {
        let stop = event(serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "s",
            "last_assistant_message": "done",
        }));
        assert_eq!(
            message(&stop, &p(), " 12s").markdown,
            "**p** `s` 12s  #claude\n\ndone"
        );
    }

    #[test]
    fn a_prompt_reports_what_was_asked_and_opens_the_thread() {
        let submit = event(serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "s",
            "prompt": "what does it do",
        }));
        assert_eq!(
            message(&submit, &p(), "").markdown,
            "**p** `s`\n\n>what does it do"
        );
    }

    #[test]
    fn every_line_of_a_quoted_prompt_carries_its_own_marker() {
        assert_eq!(quote("one\ntwo"), ">one\n>two");
        assert_eq!(quote(""), "");
        // A prompt reads as it was typed, not as the markdown it happens to carry.
        assert_eq!(quote("run *.rs & <b>"), r">run \*\.rs &amp; &lt;b\>");
    }

    #[test]
    fn an_unread_event_reports_what_it_carries_without_the_boilerplate() {
        let odd = event(serde_json::json!({
            "hook_event_name": "PreCompact",
            "session_id": "s",
            "transcript_path": "/tmp/t.jsonl",
            "reason": "clear",
        }));
        assert_eq!(
            message(&odd, &p(), "").markdown,
            "**p** `s`  #claude #PreCompact\n\n\\{\"reason\":\"clear\"\\}"
        );
    }

    #[test]
    fn a_compaction_quotes_its_summary_folded_and_clipped() {
        let compacted = |trigger: &str, summary: &str| {
            message(
                &event(serde_json::json!({
                    "hook_event_name": "PostCompact",
                    "session_id": "s",
                    "trigger": trigger,
                    "compact_summary": summary,
                })),
                &p(),
                "",
            )
            .markdown
        };
        assert_eq!(
            compacted(
                "manual",
                "<analysis>\nwhy\n</analysis>\n\n<summary>\n1. a < b\n\n2. done\n</summary>"
            ),
            "**p** `s`  #claude #compact\n\n\
             <blockquote expandable>1. a &lt; b<br><br>2. done<cite>summary</cite></blockquote>\n\n\
             <blockquote expandable>why<cite>analysis</cite></blockquote>"
        );
        // The summary fills the message, up to an escape that would overrun it, so the
        // reasoning is left out.
        let long = compacted(
            "auto",
            &format!(
                "<analysis>why</analysis><summary>{}</summary>",
                "<".repeat(MAX_BYTES)
            ),
        );
        assert!((MAX_BYTES - 3..=MAX_BYTES).contains(&long.len()));
        assert!(long.ends_with("&lt;\u{2026}<cite>summary</cite></blockquote>"));
        let reasoned = compacted(
            "auto",
            &format!(
                "<analysis>{}</analysis><summary>s</summary>",
                "x".repeat(MAX_BYTES)
            ),
        );
        let analysis = reasoned.split("\n\n").last().expect("the reasoning");
        assert_eq!(analysis.len(), ANALYSIS_MAX);
        // A character takes up to four bytes, and the message holds the bytes.
        let wide = compacted(
            "auto",
            &format!("<summary>{}</summary>", "字".repeat(MAX_BYTES)),
        );
        assert!((MAX_BYTES - 3..=MAX_BYTES).contains(&wide.len()));
    }

    #[test]
    fn a_failure_without_an_error_field_still_reports() {
        let failed = event(serde_json::json!({
            "hook_event_name": "StopFailure",
            "session_id": "s",
            "reason": "overloaded",
        }));
        assert_eq!(
            message(&failed, &p(), "").markdown,
            "**p** `s`  #claude #failed\n\n\\{\"reason\":\"overloaded\"\\}"
        );
    }
}

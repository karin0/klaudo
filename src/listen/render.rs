//! How the tool calls of a turn read in the chat.

use std::time::Duration;

use crate::hook;

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
/// How long the finished calls of a run read together before they are folded: three
/// lines of a phone's screen, which is what a folded quotation still shows.
const FOLD_OVER: usize = 120;

/// How far a line summing up what a call produced or why it failed is kept, past which
/// it stops reading at a glance.
pub(super) const SUMMARY_MAX: usize = 160;

/// How far a call's description, subject and summary read.
struct Room {
    subject: usize,
    summary: usize,
}

/// The calls in view under a folded run are what a reader follows a turn by, so they
/// read as far as they are kept.
const OPEN: Room = Room {
    subject: hook::SUBJECT_MAX,
    summary: SUMMARY_MAX,
};
/// The calls ahead of them read three lines of a phone's screen for a command and one
/// for a summary.
const AHEAD: Room = Room {
    subject: 120,
    summary: 60,
};
/// How far an MCP call's arguments and its result read. They are what a reader opens
/// such a call for, and the measured calls fit these at the arguments' maximum and the
/// result's ninetieth percentile, images aside.
pub(super) const ARGUMENTS_MAX: usize = 500;
pub(super) const RESULT_MAX: usize = 1500;

/// How a tool call went, how long it took getting there, and what it produced or why
/// it failed, empty where it reported nothing.
pub(super) enum Outcome {
    Running,
    Done(Duration, String),
    Failed(Duration, String),
}

/// One tool call, from the event that announced it to the one that said how it went.
pub(super) struct Call {
    pub(super) id: String,
    /// The subagent that made it, absent on the main thread.
    pub(super) agent: Option<String>,
    pub(super) name: String,
    /// What the call says it is doing, empty where its tool describes no call of itself.
    pub(super) description: String,
    pub(super) subject: String,
    /// An MCP call's arguments, shown whole with its result in blocks of their own,
    /// absent for any other call, whose result is a line.
    pub(super) arguments: Option<String>,
    pub(super) outcome: Outcome,
}

impl Call {
    /// A mark for how it went, the tool and what it says it is doing, and the time it
    /// took. What it is working on and what it produced go on lines under that,
    /// where the eye finds them and a long command wraps without pushing the time away.
    /// The tool and the time are bold, so the eye finds a call and its cost down a run
    /// of lines whose middles are of every length.
    fn line(&self, markup: &Markup, room: &Room) -> String {
        let (mark, took, result) = match &self.outcome {
            Outcome::Running => (RUNNING, String::new(), ""),
            Outcome::Done(took, result) => (
                DONE,
                format!(" {}", (markup.bold)(&spent(*took))),
                result.as_str(),
            ),
            Outcome::Failed(took, why) => (
                FAILED,
                format!(" {}", (markup.bold)(&spent(*took))),
                why.as_str(),
            ),
        };
        let shown = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        let (arguments, result) = match &self.arguments {
            Some(arguments) => (
                shown(arguments).map(|text| (markup.block)(&text)),
                shown(result).map(|text| (markup.block)(&text)),
            ),
            None => (
                None,
                shown(result)
                    .map(|text| format!("⎿ {}", (markup.text)(&first_line(&text, room.summary)))),
            ),
        };
        let agent = match &self.agent {
            Some(agent) => format!("[{agent}] "),
            None => String::new(),
        };
        // A description is prose, while what the call works on is a path or a command
        // and keeps the span that carries it verbatim. Two spaces after the tool, which
        // is what holds its name apart from the words that follow it.
        let description = first_line(&self.description, room.subject);
        let subject = first_line(&self.subject, room.subject);
        let (said, under) = match (description.as_str(), subject.as_str()) {
            ("", "") => (String::new(), None),
            ("", subject) => (format!("  {}", (markup.code)(subject)), None),
            (description, "") => (format!("  {}", (markup.text)(description)), None),
            (description, subject) => (
                format!("  {}", (markup.text)(description)),
                Some((markup.code)(subject)),
            ),
        };
        let head = format!("{mark} {agent}{}{said}{took}", (markup.bold)(&self.name));
        [
            Some(head),
            under.map(|under| format!("⎿ {under}")),
            arguments,
            result,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(markup.newline)
    }

    /// How long the call reads ahead of the calls in view, by the words that wrap on a
    /// phone.
    fn length(&self) -> usize {
        let result = match &self.outcome {
            Outcome::Running => "",
            Outcome::Done(_, text) | Outcome::Failed(_, text) => text,
        };
        let result = match self.arguments {
            Some(_) => result.chars().count(),
            None => first_line(result, AHEAD.summary).chars().count(),
        };
        [&self.description, &self.subject]
            .iter()
            .map(|text| first_line(text, AHEAD.subject).chars().count())
            .chain([
                self.arguments
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .count(),
                result,
            ])
            .sum()
    }
}

/// How a call's line is written: as the markdown a message is, or as HTML, which is
/// all a folded quotation parses inside it.
struct Markup {
    bold: fn(&str) -> String,
    code: fn(&str) -> String,
    block: fn(&str) -> String,
    pub(super) text: fn(&str) -> String,
    newline: &'static str,
}

const MARKDOWN: Markup = Markup {
    bold: |text| format!("**{}**", hook::prose(text)),
    code,
    block: fence,
    text: hook::prose,
    newline: BREAK,
};

const HTML: Markup = Markup {
    bold: |text| format!("<b>{}</b>", hook::html(text)),
    code: |text| format!("<code>{}</code>", hook::html(text)),
    block: |text| format!("<pre>{}</pre>", hook::html(text)),
    text: hook::html,
    newline: "<br>",
};

/// The longest run of backticks in `text`, which a code span or block around it has to
/// outlast.
fn ticks(text: &str) -> usize {
    let mut longest = 0;
    let mut ticks = 0;
    for character in text.chars() {
        ticks = if character == '`' { ticks + 1 } else { 0 };
        longest = longest.max(ticks);
    }
    longest
}

/// A code block, fenced by at least the three backticks markdown opens one with.
fn fence(text: &str) -> String {
    let fence = "`".repeat((ticks(text) + 1).max(3));
    format!("{fence}\n{text}\n{fence}")
}

/// A command reads as markdown where the message is markdown, so it travels as a code
/// span, whose backticks have to outlast any run of them the command carries.
pub(super) fn code(subject: &str) -> String {
    let longest = ticks(subject);
    let fence = "`".repeat(longest + 1);
    // A span whose text opens or closes with a backtick needs the padding to keep it.
    match longest {
        0 => format!("{fence}{subject}{fence}"),
        _ => format!("{fence} {subject} {fence}"),
    }
}

/// A run of tool calls, a line per call. The oldest are counted rather than listed past
/// the cap, so what is running now stays in a message Telegram will take. The calls
/// from the first one still running, or else the latest call, stay in view, and the
/// calls ahead of them fold into one quotation once they read long.
pub(super) fn listing(calls: &[Call]) -> String {
    let elided = calls.len().saturating_sub(RUN_MAX);
    let shown = &calls[elided..];
    let first_open = shown
        .iter()
        .position(|call| matches!(call.outcome, Outcome::Running))
        .unwrap_or(shown.len())
        .min(shown.len().saturating_sub(1));
    let (ahead, open) = shown.split_at(first_open);
    let folded = ahead.iter().map(Call::length).sum::<usize>() > FOLD_OVER;
    let markup = if folded { &HTML } else { &MARKDOWN };
    let ahead = (elided > 0)
        .then(|| format!("… {elided} earlier"))
        .into_iter()
        .chain(ahead.iter().map(|call| call.line(markup, &AHEAD)));
    let open: Vec<String> = open
        .iter()
        .map(|call| call.line(&MARKDOWN, &OPEN))
        .collect();
    if !folded {
        return ahead.chain(open).collect::<Vec<_>>().join(BREAK);
    }
    let quote = format!(
        "<blockquote expandable>{}</blockquote>",
        ahead.collect::<Vec<_>>().join(HTML.newline)
    );
    // A block HTML tag runs to the next blank line, so one keeps the quotation apart
    // from the markdown under it.
    format!("{quote}\n\n{}", open.join(BREAK))
}

/// How long a tool call took, in the units a tool call runs in.
fn spent(elapsed: Duration) -> String {
    match elapsed.as_millis() {
        millis @ ..1000 => format!("{millis}ms"),
        _ => took(elapsed).trim().to_owned(),
    }
}

/// The first line of `text`, cut to `max` characters and an ellipsis past that.
pub(super) fn first_line(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or_default().trim();
    if first.chars().count() <= max {
        return first.to_owned();
    }
    first.chars().take(max).chain("\u{2026}".chars()).collect()
}

/// `text` without its blank lines, cut to `max` characters and an ellipsis past that.
/// The folded calls are one block HTML tag, which a blank line would end.
pub(super) fn clip(text: &str, max: usize) -> String {
    let kept = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if kept.chars().count() <= max {
        return kept;
    }
    kept.chars().take(max).chain("\u{2026}".chars()).collect()
}

pub(super) fn took(elapsed: Duration) -> String {
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

    fn call(name: &str, subject: &str, outcome: Outcome) -> Call {
        Call {
            id: name.to_owned(),
            agent: None,
            name: name.to_owned(),
            description: String::new(),
            subject: subject.to_owned(),
            arguments: None,
            outcome,
        }
    }

    #[test]
    fn a_call_reads_as_its_tool_its_subject_and_how_it_went() {
        assert_eq!(
            call("Read", "src/listen.rs", Outcome::Running).line(&MARKDOWN, &OPEN),
            "○ **Read**  `src/listen.rs`"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_millis(1400), String::new())
            )
            .line(&MARKDOWN, &OPEN),
            "● **Bash**  `cargo test` **1s**"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_millis(12), String::new())
            )
            .line(&MARKDOWN, &OPEN),
            "● **Bash**  `cargo test` **12ms**"
        );
        // What a failure reported reads on a line of its own.
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Failed(Duration::from_secs(4), "Exit code 1".to_owned())
            )
            .line(&MARKDOWN, &OPEN),
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
            described.line(&MARKDOWN, &OPEN),
            "× **Bash**  run the tests **4s**  \n⎿ `cargo test`  \n⎿ Exit code 1"
        );
        let markup = Call {
            description: "find *.rs in _src_".to_owned(),
            ..call("Grep", "fn seal", Outcome::Running)
        };
        assert_eq!(
            markup.line(&MARKDOWN, &OPEN),
            "○ **Grep**  find \\*\\.rs in \\_src\\_  \n⎿ `fn seal`"
        );
        let subagent = Call {
            agent: Some("Explore".to_owned()),
            ..call("Grep", "fn seal", Outcome::Running)
        };
        assert_eq!(
            subagent.line(&MARKDOWN, &OPEN),
            "○ [Explore] **Grep**  `fn seal`"
        );
        // An MCP tool's name is written in the markdown it would otherwise be read as.
        assert_eq!(
            call("mcp__telegram__send", "", Outcome::Running).line(&MARKDOWN, &OPEN),
            "○ **mcp\\_\\_telegram\\_\\_send**"
        );
    }

    #[test]
    fn a_finished_call_sums_up_what_it_produced_on_a_line_of_its_own() {
        let done = call(
            "Read",
            "src/listen.rs",
            Outcome::Done(Duration::from_millis(12), "120 lines".to_owned()),
        );
        assert_eq!(
            done.line(&MARKDOWN, &OPEN),
            "● **Read**  `src/listen.rs` **12ms**  \n⎿ 120 lines"
        );
    }

    #[test]
    fn an_mcp_call_shows_its_arguments_and_result_in_blocks() {
        let mcp = |outcome| Call {
            arguments: Some(r#"{"chat":"x"}"#.to_owned()),
            ..call("mcp__telegram__history", "", outcome)
        };
        assert_eq!(
            mcp(Outcome::Running).line(&MARKDOWN, &OPEN),
            "○ **mcp\\_\\_telegram\\_\\_history**  \n```\n{\"chat\":\"x\"}\n```"
        );
        let done = mcp(Outcome::Done(
            Duration::from_secs(1),
            "a < b\n```".to_owned(),
        ));
        assert_eq!(
            done.line(&MARKDOWN, &OPEN),
            "● **mcp\\_\\_telegram\\_\\_history** **1s**  \n```\n{\"chat\":\"x\"}\n```  \n\
             ````\na < b\n```\n````"
        );
        assert_eq!(
            done.line(&HTML, &OPEN),
            "● <b>mcp__telegram__history</b> <b>1s</b><br>\
             <pre>{\"chat\":\"x\"}</pre><br><pre>a &lt; b\n```</pre>"
        );
        // A call with no arguments and an empty result shows neither.
        let bare = Call {
            arguments: Some(String::new()),
            ..call(
                "mcp__x__y",
                "",
                Outcome::Done(Duration::from_secs(1), String::new()),
            )
        };
        assert_eq!(bare.line(&HTML, &OPEN), "● <b>mcp__x__y</b> <b>1s</b>");
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
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_secs(1), String::new()),
            ),
        ]);
        assert_eq!(
            listed,
            "○ **Read**  `src/listen.rs`  \n● **Bash**  `cargo test` **1s**"
        );
    }

    #[test]
    fn calls_ahead_of_the_running_or_the_latest_fold_once_they_read_long() {
        let done = |subject: &str| {
            call(
                "Bash",
                subject,
                Outcome::Done(Duration::from_secs(1), String::new()),
            )
        };
        let x = "x".repeat(FOLD_OVER / 2);
        let listed = listing(&[
            Call {
                description: "a < b".to_owned(),
                ..done(&x)
            },
            done(&x),
            call("Read", "src/listen.rs", Outcome::Running),
            done("ls"),
        ]);
        assert_eq!(
            listed,
            format!(
                "<blockquote expandable>\
                 ● <b>Bash</b>  a &lt; b <b>1s</b><br>⎿ <code>{x}</code><br>\
                 ● <b>Bash</b>  <code>{x}</code> <b>1s</b>\
                 </blockquote>\n\n\
                 ○ **Read**  `src/listen.rs`  \n\
                 ● **Bash**  `ls` **1s**"
            )
        );
        // The latest call stays in view once the run has finished.
        assert!(
            listing(&[done(&x), done(&x), done("x"), done("ls")])
                .ends_with("</blockquote>\n\n● **Bash**  `ls` **1s**")
        );
        // Calls ahead that read short stay on their lines.
        assert_eq!(
            listing(&[done(&x), call("Read", &x, Outcome::Running)]),
            format!("● **Bash**  `{x}` **1s**  \n○ **Read**  `{x}`")
        );
    }

    #[test]
    fn the_calls_in_view_read_further_than_those_behind() {
        let long = "y".repeat(hook::SUBJECT_MAX);
        let summary = "z".repeat(SUMMARY_MAX);
        let done = || {
            call(
                "Bash",
                &long,
                Outcome::Done(Duration::from_secs(1), summary.clone()),
            )
        };
        let listed = listing(&[done(), done()]);
        let (ahead, open) = listed.split_once("\n\n").expect("a folded run");
        assert_eq!(
            ahead,
            format!(
                "<blockquote expandable>● <b>Bash</b>  <code>{}…</code> <b>1s</b><br>⎿ {}…</blockquote>",
                &long[..AHEAD.subject],
                &summary[..AHEAD.summary],
            )
        );
        assert_eq!(open, format!("● **Bash**  `{long}` **1s**  \n⎿ {summary}"));
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
    fn a_clipped_text_keeps_its_lines_but_not_the_blank_ones() {
        assert_eq!(clip("a\n\n  \nb\n", 5), "a\nb");
        assert_eq!(clip("abcdef", 5), "abcde\u{2026}");
    }

    #[test]
    fn a_first_line_is_cut_to_its_room() {
        assert_eq!(first_line(" Exit code 1\nError: nope", 60), "Exit code 1");
        assert_eq!(first_line(&"x".repeat(9), 5), "xxxxx\u{2026}");
        assert_eq!(first_line("xxxxx", 5), "xxxxx");
    }
}

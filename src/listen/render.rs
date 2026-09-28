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
/// The first line of what a failed tool reported, past which it stops reading at a
/// glance.
const WHY_MAX: usize = 60;

/// How a tool call went, and how long it took getting there.
pub(super) enum Outcome {
    Running,
    Done(Duration),
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
    pub(super) outcome: Outcome,
}

impl Call {
    /// A mark for how it went, the tool and what it says it is doing, and the time it
    /// took. What it is working on and what a failure reported go on lines under that,
    /// where the eye finds them and a long command wraps without pushing the time away.
    /// The tool and the time are bold, so the eye finds a call and its cost down a run
    /// of lines whose middles are of every length.
    fn line(&self, markup: &Markup) -> String {
        let (mark, took, why) = match &self.outcome {
            Outcome::Running => (RUNNING, String::new(), None),
            Outcome::Done(took) => (DONE, format!(" {}", (markup.bold)(&spent(*took))), None),
            Outcome::Failed(took, why) => (
                FAILED,
                format!(" {}", (markup.bold)(&spent(*took))),
                Some(why),
            ),
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
            why.map(|why| format!("⎿ {}", (markup.text)(why))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(markup.newline)
    }

    /// How long the call reads, by the words that wrap on a phone.
    fn length(&self) -> usize {
        self.description.chars().count() + self.subject.chars().count()
    }
}

/// How a call's line is written: as the markdown a message is, or as HTML, which is
/// all a folded quotation parses inside it.
struct Markup {
    bold: fn(&str) -> String,
    code: fn(&str) -> String,
    pub(super) text: fn(&str) -> String,
    newline: &'static str,
}

const MARKDOWN: Markup = Markup {
    bold: |text| format!("**{text}**"),
    code,
    text: hook::prose,
    newline: BREAK,
};

const HTML: Markup = Markup {
    bold: |text| format!("<b>{}</b>", hook::html(text)),
    code: |text| format!("<code>{}</code>", hook::html(text)),
    text: hook::html,
    newline: "<br>",
};

/// A command reads as markdown where the message is markdown, so it travels as a code
/// span, whose backticks have to outlast any run of them the command carries.
pub(super) fn code(subject: &str) -> String {
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

/// A run of tool calls, a line per call. The oldest are counted rather than listed past
/// the cap, so what is running now stays in a message Telegram will take. The calls
/// that finished ahead of the first one still running fold into one quotation once
/// they read long, leaving what a reader is waiting on in view.
pub(super) fn listing(calls: &[Call]) -> String {
    let elided = calls.len().saturating_sub(RUN_MAX);
    let shown = &calls[elided..];
    let running = shown
        .iter()
        .position(|call| matches!(call.outcome, Outcome::Running))
        .unwrap_or(shown.len());
    let (finished, open) = shown.split_at(running);
    let folded = finished.iter().map(Call::length).sum::<usize>() > FOLD_OVER;
    let markup = if folded { &HTML } else { &MARKDOWN };
    let early = (elided > 0)
        .then(|| format!("… {elided} earlier"))
        .into_iter()
        .chain(finished.iter().map(|call| call.line(markup)));
    let open: Vec<String> = open.iter().map(|call| call.line(&MARKDOWN)).collect();
    if !folded {
        return early.chain(open).collect::<Vec<_>>().join(BREAK);
    }
    let quote = format!(
        "<blockquote expandable>{}</blockquote>",
        early.collect::<Vec<_>>().join(HTML.newline)
    );
    // A block HTML tag runs to the next blank line, so one keeps the quotation apart
    // from the markdown under it.
    std::iter::once(quote)
        .chain((!open.is_empty()).then(|| open.join(BREAK)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// How long a tool call took, in the units a tool call runs in.
fn spent(elapsed: Duration) -> String {
    match elapsed.as_millis() {
        millis @ ..1000 => format!("{millis}ms"),
        _ => took(elapsed).trim().to_owned(),
    }
}

/// What a failed tool reported, in one line.
pub(super) fn why(error: &str) -> String {
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
            outcome,
        }
    }

    #[test]
    fn a_call_reads_as_its_tool_its_subject_and_how_it_went() {
        assert_eq!(
            call("Read", "src/listen.rs", Outcome::Running).line(&MARKDOWN),
            "○ **Read**  `src/listen.rs`"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_millis(1400))
            )
            .line(&MARKDOWN),
            "● **Bash**  `cargo test` **1s**"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Done(Duration::from_millis(12))
            )
            .line(&MARKDOWN),
            "● **Bash**  `cargo test` **12ms**"
        );
        // What a failure reported reads on a line of its own.
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Failed(Duration::from_secs(4), "Exit code 1".to_owned())
            )
            .line(&MARKDOWN),
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
            described.line(&MARKDOWN),
            "× **Bash**  run the tests **4s**  \n⎿ `cargo test`  \n⎿ Exit code 1"
        );
        let markup = Call {
            description: "find *.rs in _src_".to_owned(),
            ..call("Grep", "fn seal", Outcome::Running)
        };
        assert_eq!(
            markup.line(&MARKDOWN),
            "○ **Grep**  find \\*\\.rs in \\_src\\_  \n⎿ `fn seal`"
        );
        let subagent = Call {
            agent: Some("Explore".to_owned()),
            ..call("Grep", "fn seal", Outcome::Running)
        };
        assert_eq!(subagent.line(&MARKDOWN), "○ [Explore] **Grep**  `fn seal`");
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
    fn finished_calls_fold_once_they_read_long_together() {
        let done = |subject: &str| call("Bash", subject, Outcome::Done(Duration::from_secs(1)));
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
        // A run that has all finished folds whole.
        let whole = listing(&[done(&x), done(&x), done("ls")]);
        assert!(whole.starts_with("<blockquote expandable>") && whole.ends_with("</blockquote>"));
        // Finished calls that read short stay on their lines.
        assert_eq!(
            listing(&[done(&x), call("Read", &x, Outcome::Running)]),
            format!("● **Bash**  `{x}` **1s**  \n○ **Read**  `{x}`")
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
}

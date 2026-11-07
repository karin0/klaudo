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

/// How far a call's description, subject and summary read, and whether what it
/// printed reads whole or as the one line that matters most.
struct Room {
    subject: usize,
    summary: usize,
    whole: bool,
}

/// The calls in view under a folded run are what a reader follows a turn by, so they
/// read as far as they are kept.
const OPEN: Room = Room {
    subject: hook::SUBJECT_MAX,
    summary: SUMMARY_MAX,
    whole: true,
};
/// The calls ahead of them read three lines of a phone's screen for a command and one
/// for a summary.
const AHEAD: Room = Room {
    subject: 120,
    summary: 60,
    whole: false,
};
/// How far an MCP call's arguments and its result read. They are what a reader opens
/// such a call for, and the measured calls fit these at the arguments' maximum and the
/// result's ninetieth percentile, images aside.
pub(super) const ARGUMENTS_MAX: usize = 500;
pub(super) const RESULT_MAX: usize = 1500;

/// How a tool call went, how long it took getting there, and what it produced or why
/// it failed, empty where it reported nothing. What a call printed and why it failed
/// read in a code span per line, which a phone wraps.
pub(super) enum Outcome {
    Running,
    /// The last lines a command printed, or an MCP tool's text.
    Printed(Duration, String),
    /// What Claude Code's terminal sums the call up with.
    Summed(Duration, String),
    Failed(Duration, String),
}

impl Outcome {
    fn took(&self) -> Option<Duration> {
        match self {
            Self::Running => None,
            Self::Printed(took, _) | Self::Summed(took, _) | Self::Failed(took, _) => Some(*took),
        }
    }
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
        let mark = match self.outcome {
            Outcome::Running => RUNNING,
            Outcome::Printed(..) | Outcome::Summed(..) => DONE,
            Outcome::Failed(..) => FAILED,
        };
        let took = self
            .outcome
            .took()
            .map(|took| format!(" {}", (markup.bold)(&spent(took))))
            .unwrap_or_default();
        let printed = |text: &str| {
            text.lines()
                .map(|line| (markup.code)(&indented(line)))
                .collect::<Vec<_>>()
                .join(markup.newline)
        };
        let arguments = self
            .arguments
            .as_deref()
            .filter(|arguments| !arguments.is_empty())
            .map(|arguments| format!("⎿ {}", printed(arguments)));
        let result = self.produced(room).map(|(text, code)| {
            format!(
                "⎿ {}",
                if code {
                    printed(&text)
                } else {
                    (markup.text)(&text)
                }
            )
        });
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

    /// What the call produced as `room` shows it, and whether it is text the call
    /// printed or an error, which read as code. An MCP call's text is what a reader
    /// opens it for, so it reads whole anywhere, and any other call ahead of the calls
    /// in view shows the line a reader looks for first: the last a command printed, or
    /// the first of an error.
    fn produced(&self, room: &Room) -> Option<(String, bool)> {
        let (text, last) = match &self.outcome {
            Outcome::Running => return None,
            Outcome::Summed(_, text) => {
                return (!text.is_empty()).then(|| (first_line(text, room.summary), false));
            }
            Outcome::Printed(_, text) => (text, true),
            Outcome::Failed(_, text) => (text, false),
        };
        let text = if room.whole || self.arguments.is_some() {
            text.clone()
        } else {
            let line = if last {
                text.lines().last()
            } else {
                text.lines().next()
            };
            first_line(line.unwrap_or_default(), room.summary)
        };
        (!text.is_empty()).then_some((text, true))
    }

    /// How long the call reads ahead of the calls in view, by the words that wrap on a
    /// phone.
    fn length(&self) -> usize {
        [&self.description, &self.subject]
            .iter()
            .map(|text| first_line(text, AHEAD.subject))
            .chain(self.arguments.clone())
            .chain(self.produced(&AHEAD).map(|(text, _)| text))
            .map(|text| text.chars().count())
            .sum()
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
    bold: |text| format!("**{}**", hook::prose(text)),
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
/// the cap, so what is running now stays in a message Telegram will take. While the run
/// is `live`, the calls from the first one still running, or else the latest call, stay
/// in view, and the calls ahead of them fold into one quotation once they read long. A
/// run the turn has moved on from has no calls in view.
pub(super) fn listing(calls: &[Call], live: bool) -> String {
    let elided = calls.len().saturating_sub(RUN_MAX);
    let shown = &calls[elided..];
    let first_open = if live {
        shown
            .iter()
            .position(|call| matches!(call.outcome, Outcome::Running))
            .unwrap_or(shown.len())
            .min(shown.len().saturating_sub(1))
    } else {
        shown.len()
    };
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
    std::iter::once(quote)
        .chain((!open.is_empty()).then(|| open.join(BREAK)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `line` with the blanks it opens with as ideographic spaces of two columns each,
/// since Telegram drops the spaces a code span opens with and keeps those. A tab counts
/// as four columns, and an odd column stays a space after them.
fn indented(line: &str) -> String {
    let body = line.trim_start_matches([' ', '\t']);
    let columns: usize = line[..line.len() - body.len()]
        .chars()
        .map(|blank| if blank == '\t' { 4 } else { 1 })
        .sum();
    format!(
        "{}{}{body}",
        "\u{3000}".repeat(columns / 2),
        " ".repeat(columns % 2)
    )
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
                Outcome::Summed(Duration::from_millis(1400), String::new())
            )
            .line(&MARKDOWN, &OPEN),
            "● **Bash**  `cargo test` **1s**"
        );
        assert_eq!(
            call(
                "Bash",
                "cargo test",
                Outcome::Summed(Duration::from_millis(12), String::new())
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
            "× **Bash**  `cargo test` **4s**  \n⎿ `Exit code 1`"
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
            "× **Bash**  run the tests **4s**  \n⎿ `cargo test`  \n⎿ `Exit code 1`"
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
            Outcome::Summed(Duration::from_millis(12), "120 lines".to_owned()),
        );
        assert_eq!(
            done.line(&MARKDOWN, &OPEN),
            "● **Read**  `src/listen.rs` **12ms**  \n⎿ 120 lines"
        );
    }

    #[test]
    fn printed_lines_read_whole_in_view_and_as_one_line_ahead() {
        let printed = call(
            "Bash",
            "cargo test",
            Outcome::Printed(
                Duration::from_secs(1),
                "running\ntest result: ok".to_owned(),
            ),
        );
        assert_eq!(
            printed.line(&MARKDOWN, &OPEN),
            "● **Bash**  `cargo test` **1s**  \n⎿ `running`  \n`test result: ok`"
        );
        // Ahead, a command shows the last line it printed, and an error its first.
        assert_eq!(
            printed.line(&HTML, &AHEAD),
            "● <b>Bash</b>  <code>cargo test</code> <b>1s</b><br>⎿ <code>test result: ok</code>"
        );
        let failed = call(
            "Bash",
            "cargo test",
            Outcome::Failed(
                Duration::from_secs(1),
                "Exit code 1\nerror[E0425]".to_owned(),
            ),
        );
        assert_eq!(
            failed.line(&HTML, &AHEAD),
            "× <b>Bash</b>  <code>cargo test</code> <b>1s</b><br>⎿ <code>Exit code 1</code>"
        );
    }

    #[test]
    fn an_mcp_call_shows_its_arguments_and_result_a_line_of_code_at_a_time() {
        let mcp = |outcome| Call {
            arguments: Some(r#"{"chat":"x"}"#.to_owned()),
            ..call("mcp__telegram__history", "", outcome)
        };
        assert_eq!(
            mcp(Outcome::Running).line(&MARKDOWN, &OPEN),
            "○ **mcp\\_\\_telegram\\_\\_history**  \n⎿ `{\"chat\":\"x\"}`"
        );
        let done = mcp(Outcome::Printed(
            Duration::from_secs(1),
            "a < b\n```".to_owned(),
        ));
        assert_eq!(
            done.line(&MARKDOWN, &OPEN),
            "● **mcp\\_\\_telegram\\_\\_history** **1s**  \n⎿ `{\"chat\":\"x\"}`  \n\
             ⎿ `a < b`  \n```` ``` ````"
        );
        assert_eq!(
            done.line(&HTML, &OPEN),
            "● <b>mcp__telegram__history</b> <b>1s</b><br>\
             ⎿ <code>{\"chat\":\"x\"}</code><br>⎿ <code>a &lt; b</code><br><code>```</code>"
        );
        // A call with no arguments and an empty result shows neither.
        let bare = Call {
            arguments: Some(String::new()),
            ..call(
                "mcp__x__y",
                "",
                Outcome::Printed(Duration::from_secs(1), String::new()),
            )
        };
        assert_eq!(bare.line(&HTML, &OPEN), "● <b>mcp__x__y</b> <b>1s</b>");
    }

    #[test]
    fn printed_indentation_survives_as_ideographic_spaces() {
        assert_eq!(indented("    a  b"), "\u{3000}\u{3000}a  b");
        assert_eq!(indented("\t   a"), "\u{3000}\u{3000}\u{3000} a");
        assert_eq!(indented("a"), "a");
        let printed = call(
            "Bash",
            "python3 x.py",
            Outcome::Failed(Duration::from_secs(1), "Traceback\n  File x.py".to_owned()),
        );
        assert_eq!(
            printed.line(&MARKDOWN, &OPEN),
            "× **Bash**  `python3 x.py` **1s**  \n⎿ `Traceback`  \n`\u{3000}File x.py`"
        );
    }

    #[test]
    fn a_command_travels_in_a_span_past_any_backticks_it_carries() {
        assert_eq!(code("cargo test"), "`cargo test`");
        assert_eq!(code("echo ```x```"), "```` echo ```x``` ````");
    }

    #[test]
    fn a_run_keeps_its_calls_on_lines_of_their_own() {
        let listed = listing(
            &[
                call("Read", "src/listen.rs", Outcome::Running),
                call(
                    "Bash",
                    "cargo test",
                    Outcome::Summed(Duration::from_secs(1), String::new()),
                ),
            ],
            true,
        );
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
                Outcome::Summed(Duration::from_secs(1), String::new()),
            )
        };
        let x = "x".repeat(FOLD_OVER / 2);
        let listed = listing(
            &[
                Call {
                    description: "a < b".to_owned(),
                    ..done(&x)
                },
                done(&x),
                call("Read", "src/listen.rs", Outcome::Running),
                done("ls"),
            ],
            true,
        );
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
            listing(&[done(&x), done(&x), done("x"), done("ls")], true)
                .ends_with("</blockquote>\n\n● **Bash**  `ls` **1s**")
        );
        // A run the turn has moved on from folds whole, and short it reads a line a call.
        let sealed = listing(&[done(&x), done(&x), done("x"), done("ls")], false);
        assert!(sealed.starts_with("<blockquote expandable>") && sealed.ends_with("</blockquote>"));
        let printed = || {
            call(
                "Bash",
                "ls",
                Outcome::Printed(Duration::from_secs(1), "a\nb".to_owned()),
            )
        };
        assert_eq!(
            listing(&[printed()], false),
            "● **Bash**  `ls` **1s**  \n⎿ `b`"
        );
        // Calls ahead that read short stay on their lines.
        assert_eq!(
            listing(&[done(&x), call("Read", &x, Outcome::Running)], true),
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
                Outcome::Summed(Duration::from_secs(1), summary.clone()),
            )
        };
        let listed = listing(&[done(), done()], true);
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
        let listed = listing(&calls, true);
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
    fn a_first_line_is_cut_to_its_room() {
        assert_eq!(first_line(" Exit code 1\nError: nope", 60), "Exit code 1");
        assert_eq!(first_line(&"x".repeat(9), 5), "xxxxx\u{2026}");
        assert_eq!(first_line("xxxxx", 5), "xxxxx");
    }
}

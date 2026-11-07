//! What status lines report of the context and the plan's limits, which `/usage` and
//! the answer closing a turn show.

use std::time::Duration;

use serde::Deserialize;

use kuriero::Message;

use crate::telegram::Admitted;

use super::chat::{NEW, ago};
use super::{Machine, Session, now_millis};

/// What Claude Code hands a session's status line, as `klaudo status` forwards it. Both
/// parts are absent until the session's first API call returns.
#[derive(Deserialize)]
pub(super) struct Status {
    session_id: String,
    context_window: Option<Window>,
    rate_limits: Option<Limits>,
}

#[derive(Deserialize, Clone)]
pub(super) struct Window {
    context_window_size: u64,
    current_usage: Option<Usage>,
    used_percentage: Option<f64>,
}

/// The tokens a call sent, which is what the context held.
#[derive(Deserialize, Clone)]
pub(super) struct Usage {
    #[serde(rename = "input_tokens")]
    uncached: u64,
    #[serde(rename = "cache_creation_input_tokens")]
    cache_written: u64,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read: u64,
}

impl Usage {
    pub(super) fn total(&self) -> u64 {
        self.uncached + self.cache_written + self.cache_read
    }

    /// The whole percentage of the tokens sent that were read from the cache, rounded down
    /// so that 100% means all of them.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a context of tokens stays far below 2^52"
    )]
    pub(super) fn hit(&self) -> f64 {
        (self.cache_read as f64 * 100.0 / self.total() as f64).floor()
    }
}

/// How full a session's context is: what its status line last reported, and when, in
/// Unix seconds, or else the tokens its transcript shows the latest call sent.
pub(super) enum Context<'a> {
    Reported(&'a Window, u64),
    Counted(u64),
}

impl Session {
    pub(super) fn context(&mut self) -> Option<Context<'_>> {
        match &self.window {
            Some((window, at)) => Some(Context::Reported(window, *at)),
            None => self.transcript.as_mut()?.tokens().map(Context::Counted),
        }
    }

    /// The percentage of what the first call of the turn sent that was read from the
    /// cache, which falls once the cache has expired since the turn before.
    pub(super) fn hit(&mut self) -> Option<f64> {
        self.transcript.as_mut()?.hit()
    }
}

/// The plan's limits, which every session of the account reports alike.
#[derive(Deserialize)]
pub(super) struct Limits {
    five_hour: Option<Limit>,
    seven_day: Option<Limit>,
}

#[derive(Deserialize)]
struct Limit {
    used_percentage: f64,
    /// Unix seconds.
    resets_at: u64,
}

impl Machine {
    /// A status line reports whenever the session redraws it, which is too often for the
    /// state file and cheap to wait for again after a restart.
    pub(super) fn status(&mut self, status: Status) {
        let now = now_millis() / 1000;
        if let Some(limits) = status.rate_limits {
            self.limits = Some((limits, now));
        }
        if let (Some(window), Some(session)) = (
            status.context_window,
            self.sessions.get_mut(&status.session_id),
        ) {
            session.window = Some((window, now));
        }
    }

    /// The plan's limits, and how full the context is of the session a message replying
    /// to `replied` would reach. With such a session the answer goes under its head, so a
    /// reply to the answer reaches it too. When a limit resets is written by each reader's
    /// client, in their own zone.
    pub(super) fn usage(&mut self, from: &Admitted, asked: i64, replied: Option<&Message>) {
        let place = from.place;
        let address = match self.addressee(place, replied) {
            Ok(address) => address.filter(|address| address != NEW),
            Err(error) => return self.say(place, error),
        };
        let reached = address
            .and_then(|address| self.addressed(&address))
            .map(|(id, session)| (id.clone(), session.dir.clone()));
        if let Some((_, dir)) = &reached
            && !self.reaches(from.guest.as_deref(), place, dir)
        {
            return;
        }
        let now = now_millis() / 1000;
        let mut rows = Vec::new();
        let mut ages = Vec::new();
        let mut head = None;
        if let Some((id, _)) = &reached {
            let session = self.sessions.get_mut(id).expect("a session just found");
            head = Some(session.head(id, None).html());
            rows.push(match session.context() {
                Some(Context::Reported(window, at)) => {
                    ages.push(format!("context {}", ago_since(now, at)));
                    match (&window.current_usage, window.used_percentage) {
                        (Some(usage), Some(percentage)) => format!(
                            "{}context {percentage:.0}%, {} of {}",
                            gauge(percentage),
                            tokens(usage.total()),
                            tokens(window.context_window_size),
                        ),
                        _ => "context: nothing has been sent yet".to_owned(),
                    }
                }
                Some(Context::Counted(count)) => {
                    format!("context: {}, from the transcript", tokens(count))
                }
                None => "context: not reported yet".to_owned(),
            });
        }
        match &self.limits {
            Some((limits, at)) => {
                for (name, limit) in [("5-hour", &limits.five_hour), ("7-day", &limits.seven_day)] {
                    if let Some(limit) = limit {
                        let left = Duration::from_secs(limit.resets_at.saturating_sub(now));
                        rows.push(format!(
                            "{}{name} {:.0}%, resets in {}\n{UNDER}{}",
                            gauge(limit.used_percentage),
                            limit.used_percentage,
                            until(left, " "),
                            moment(limit.resets_at, &utc(limit.resets_at)),
                        ));
                    }
                }
                ages.push(format!("limits {}", ago_since(now, *at)));
            }
            None => rows.push("limits: not reported yet".to_owned()),
        }
        if !ages.is_empty() {
            rows.push(format!("reported: {}", ages.join(", ")));
        }
        let said = rows.join("\n");
        let answer = match head {
            Some(head) => format!("{head}\n{said}"),
            None => said,
        };
        self.telegram.html(place, &answer, asked);
    }
}

/// A percentage as a bar ten cells wide, filled to the eighth of a cell, and the gap to
/// the words after it. The monospace font is what lines the blocks up from row to row.
fn gauge(percentage: f64) -> String {
    const CELLS: usize = 10;
    const PARTS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    // Clamped to the bar's eighty eighths first, so the cast neither truncates nor wraps.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let eighths = (percentage.clamp(0.0, 100.0) * 0.8).round() as usize;
    let full = eighths / 8;
    let part = PARTS.get((eighths % 8).wrapping_sub(1));
    let empty = CELLS - full - usize::from(part.is_some());
    format!(
        "<code>{}{}{}</code>  ",
        "█".repeat(full),
        part.map(char::to_string).unwrap_or_default(),
        "░".repeat(empty)
    )
}

/// What lines a row up under the words after a gauge. The gauge is in the monospace
/// font and the row in the reader's own, where twelve en spaces of half an em come
/// closest to the gauge's ten cells of about 0.6 em.
const UNDER: &str = "\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}\u{2002}  ";

/// A moment each reader's client writes in their own zone as weekday, date and time,
/// with `fallback` for a client that cannot.
fn moment(unix: u64, fallback: &str) -> String {
    format!("<tg-time unix=\"{unix}\" format=\"wDt\">{fallback}</tg-time>")
}

fn utc(unix: u64) -> String {
    format!("{:02}:{:02} UTC", unix % 86400 / 3600, unix % 3600 / 60)
}

fn ago_since(now: u64, then: u64) -> String {
    ago(Duration::from_secs(now.saturating_sub(then)))
}

/// How long until a limit resets, to the minute, with `gap` between the two units.
fn until(left: Duration, gap: &str) -> String {
    let minutes = left.as_secs().div_ceil(60);
    match minutes {
        0..60 => format!("{minutes}m"),
        60..1440 => format!("{}h{gap}{}m", minutes / 60, minutes % 60),
        _ => format!("{}d{gap}{}h", minutes / 1440, minutes % 1440 / 60),
    }
}

/// The figures under the answer that closes a turn, as one line of code: how full the
/// context is and how much of the turn's first call hit the cache, then how much of
/// each limit is used and how long until it resets.
pub(super) fn status_line(
    context: Option<Context<'_>>,
    hit: Option<f64>,
    limits: Option<&(Limits, u64)>,
) -> Option<String> {
    let now = now_millis() / 1000;
    let context = context.and_then(|context| match context {
        Context::Reported(window, _) => {
            let usage = window.current_usage.as_ref()?;
            Some(format!(
                "{:.0}% {}/{}",
                window.used_percentage?,
                tokens(usage.total()),
                tokens(window.context_window_size),
            ))
        }
        Context::Counted(count) => Some(tokens(count)),
    });
    let context = context.map(|context| match hit {
        Some(hit) => format!("{context} {hit:.0}%"),
        None => context,
    });
    let limits = limits
        .into_iter()
        .flat_map(|(limits, _)| [&limits.five_hour, &limits.seven_day])
        .flatten()
        .map(|limit| {
            let left = Duration::from_secs(limit.resets_at.saturating_sub(now));
            format!("{:.0}% {}", limit.used_percentage, until(left, ""))
        });
    let figures: Vec<String> = context.into_iter().chain(limits).collect();
    (!figures.is_empty()).then(|| format!("`{}`", figures.join(" · ")))
}

/// A token count the way Claude Code writes one, as `45.6k` or `1m`.
fn tokens(count: u64) -> String {
    let (tenths, unit) = match count {
        0..1000 => return count.to_string(),
        1000..1_000_000 => ((count + 50) / 100, "k"),
        _ => ((count + 50_000) / 100_000, "m"),
    };
    match tenths % 10 {
        0 => format!("{}{unit}", tenths / 10),
        tenth => format!("{}.{tenth}{unit}", tenths / 10),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gauge_fills_to_the_eighth_of_a_cell() {
        let bar = |percentage| {
            gauge(percentage)
                .replace("<code>", "")
                .replace("</code>  ", "")
        };
        assert_eq!(bar(0.0), "░░░░░░░░░░");
        assert_eq!(bar(1.0), "▏░░░░░░░░░");
        assert_eq!(bar(56.0), "█████▋░░░░");
        assert_eq!(bar(100.0), "██████████");
        assert_eq!(bar(120.0), "██████████");
    }

    #[test]
    fn a_reset_reads_to_the_minute() {
        assert_eq!(until(Duration::from_secs(59), " "), "1m");
        assert_eq!(until(Duration::from_mins(209), " "), "3h 29m");
        assert_eq!(until(Duration::from_hours(62), " "), "2d 14h");
    }

    #[test]
    fn a_token_count_reads_as_claude_code_writes_it() {
        assert_eq!(tokens(75), "75");
        assert_eq!(tokens(45_556), "45.6k");
        assert_eq!(tokens(1_000_000), "1m");
    }
}

//! The links that open a file an answer names, or the changes of a turn, in the Telegram
//! Mini App the daemon serves. A link carries everything the page needs in its `startapp`
//! parameter, so a link posted before a restart still opens.

use std::net::SocketAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::telegram::setting;

/// The longest `startapp` parameter Telegram passes on.
const PARAMETER_MAX: usize = 512;

/// Where the daemon serves the page, or `None` when the env file has it serve none.
pub fn listen() -> Option<SocketAddr> {
    setting("WEB_LISTEN").map(|listen| {
        listen
            .parse()
            .unwrap_or_else(|error| panic!("WEB_LISTEN {listen}: {error}"))
    })
}

/// The link to the main Mini App of the bot named `username`, which hands the `startapp`
/// parameter appended to it on to the page.
pub fn app(username: &str) -> String {
    format!("https://t.me/{username}")
}

/// How the links of one session's answer are rewritten: the Mini App link, and the
/// session a relative path is resolved in.
#[derive(Clone, Copy)]
pub struct Links<'a> {
    app: &'a str,
    session: &'a str,
}

impl<'a> Links<'a> {
    /// `None` without a Mini App to link to, and for a session id that would not survive
    /// the `startapp` alphabet, which a session id Claude Code makes, a UUID, does.
    pub fn new(app: Option<&'a str>, session: &'a str) -> Option<Self> {
        let app = app?;
        session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            .then_some(Self { app, session })
    }

    /// The Mini App link opening the file a markdown link's `destination` names, which
    /// is one with no scheme that is not a fragment of the page itself. A fragment of
    /// `L12` or `L12-L20` names the lines. A link too long for Telegram stays as it was.
    pub fn to(self, destination: &str) -> Option<String> {
        let (path, fragment) = destination.split_once('#').unwrap_or((destination, ""));
        if path.is_empty() || scheme(path) {
            return None;
        }
        let (from, to) = lines(fragment).unwrap_or((0, 0));
        self.opening(Opens::File {
            path: path.to_owned(),
            from,
            to,
        })
    }

    /// The Mini App link opening the changes of the session's working tree since the
    /// commit `since`, a full hash.
    pub fn changes(self, since: &str) -> Option<String> {
        self.opening(Opens::Changes {
            since: since.to_owned(),
        })
    }

    fn opening(self, opens: Opens) -> Option<String> {
        let parameter = Target {
            session: self.session.to_owned(),
            opens,
        }
        .encode();
        (parameter.len() <= PARAMETER_MAX).then(|| format!("{}?startapp={parameter}", self.app))
    }
}

/// Whether `path` opens with a URL scheme, as `https:` or `mailto:` do.
fn scheme(path: &str) -> bool {
    path.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// The lines a fragment such as `L12` or `L12-L20` names.
fn lines(fragment: &str) -> Option<(usize, usize)> {
    let range = fragment.strip_prefix('L')?;
    let (from, to) = range.split_once('-').unwrap_or((range, range));
    let from: usize = from.parse().ok()?;
    let to: usize = to.strip_prefix('L').unwrap_or(to).parse().ok()?;
    (from > 0 && to >= from).then_some((from, to))
}

/// What a link opens in a session. It travels as `<session>_<from>_<to>_<path in
/// base64url>` for a file and `<session>_<since>` for changes, which keeps to the
/// alphabet `startapp` allows, since a session id holds no `_`.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub session: String,
    pub opens: Opens,
}

#[derive(Debug, PartialEq)]
pub enum Opens {
    /// A file as the answer wrote it, and its lines, `0` to `0` for none.
    File {
        path: String,
        from: usize,
        to: usize,
    },
    /// The changes of the working tree since the commit `since`.
    Changes { since: String },
}

impl Target {
    fn encode(&self) -> String {
        match &self.opens {
            Opens::File { path, from, to } => format!(
                "{}_{from}_{to}_{}",
                self.session,
                URL_SAFE_NO_PAD.encode(path)
            ),
            Opens::Changes { since } => format!("{}_{since}", self.session),
        }
    }

    pub fn decode(parameter: &str) -> Option<Self> {
        let fields: Vec<&str> = parameter.splitn(4, '_').collect();
        let opens = match fields[1..] {
            [since] => Opens::Changes {
                since: since.to_owned(),
            },
            [from, to, path] => Opens::File {
                from: from.parse().ok()?,
                to: to.parse().ok()?,
                path: String::from_utf8(URL_SAFE_NO_PAD.decode(path).ok()?).ok()?,
            },
            _ => return None,
        };
        Some(Self {
            session: fields[0].to_owned(),
            opens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0b7c4bd0-62d5-4b8e-9e05-2bd0f8f0a7d3";

    fn links() -> Links<'static> {
        Links::new(Some("https://t.me/bot/app"), SESSION).expect("links")
    }

    fn opened(destination: &str) -> Option<Target> {
        let link = links().to(destination)?;
        let parameter = link.strip_prefix("https://t.me/bot/app?startapp=")?;
        assert!(
            parameter
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "{parameter} keeps to the startapp alphabet"
        );
        Target::decode(parameter)
    }

    fn target(path: &str, from: usize, to: usize) -> Target {
        Target {
            session: SESSION.to_owned(),
            opens: Opens::File {
                path: path.to_owned(),
                from,
                to,
            },
        }
    }

    #[test]
    fn a_link_to_a_file_opens_it_at_its_lines() {
        assert_eq!(
            opened("src/listen.rs#L588"),
            Some(target("src/listen.rs", 588, 588))
        );
        assert_eq!(
            opened("src/listen.rs#L12-L20"),
            Some(target("src/listen.rs", 12, 20))
        );
        assert_eq!(
            opened("/home/u/文档/a b.md"),
            Some(target("/home/u/文档/a b.md", 0, 0))
        );
        assert_eq!(
            opened("README.md#configuration"),
            Some(target("README.md", 0, 0))
        );
    }

    #[test]
    fn a_link_to_changes_opens_them_since_its_commit() {
        let since = "0123456789abcdef0123456789abcdef01234567";
        let link = links().changes(since).expect("a link");
        let parameter = link
            .strip_prefix("https://t.me/bot/app?startapp=")
            .expect("a startapp link");
        assert_eq!(
            Target::decode(parameter),
            Some(Target {
                session: SESSION.to_owned(),
                opens: Opens::Changes {
                    since: since.to_owned()
                },
            })
        );
    }

    #[test]
    fn a_link_elsewhere_stays_as_it_was() {
        for destination in ["https://example.com/a.rs", "mailto:a@b.c", "#L3", ""] {
            assert_eq!(links().to(destination), None, "{destination}");
        }
    }

    #[test]
    fn a_link_too_long_for_telegram_stays_as_it_was() {
        assert!(links().to(&"a/".repeat(170)).is_some());
        assert_eq!(links().to(&"a/".repeat(180)), None);
    }

    #[test]
    fn a_session_outside_the_startapp_alphabet_has_no_links() {
        assert!(Links::new(Some("https://t.me/bot/app"), "a_b").is_none());
        assert!(Links::new(None, SESSION).is_none());
    }
}

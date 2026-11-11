//! The links that open the files an answer names, or the changes of a turn, in the Telegram
//! Mini App the daemon serves. A link carries everything the page needs in its `startapp`
//! parameter, so a link posted before a restart still opens.

use std::net::SocketAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;

use crate::changes;
use crate::telegram::setting;

/// The longest `startapp` parameter Telegram passes on, as tma.js documents it.
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

    /// The Mini App link of each of a message's markdown link `destinations`, all opening
    /// one page of every file the message links to, each at its own file and lines. A
    /// destination names a file when it has no scheme and is not a fragment of the page
    /// itself, and a fragment of `L12` or `L12-L20` names the lines. A link whose page
    /// would be too long for Telegram opens its file alone, and stays as it was when even
    /// that is too long.
    pub fn files(self, destinations: &[&str]) -> Vec<Option<String>> {
        let named: Vec<_> = destinations.iter().map(|d| file(d)).collect();
        let mut files: Vec<Linked> = Vec::new();
        for &(path, lines) in named.iter().flatten() {
            let index = files
                .iter()
                .position(|linked| linked.path == path)
                .unwrap_or_else(|| {
                    files.push(Linked {
                        path: path.to_owned(),
                        lines: Vec::new(),
                    });
                    files.len() - 1
                });
            let linked = &mut files[index];
            if let Some(lines) = lines
                && !linked.lines.contains(&lines)
            {
                linked.lines.push(lines);
            }
        }
        let shared = page(&files);
        named
            .iter()
            .map(|named| {
                let (path, lines) = (*named)?;
                self.opening(&shared, &files, path, lines).or_else(|| {
                    let alone = [Linked {
                        path: path.to_owned(),
                        lines: lines.into_iter().collect(),
                    }];
                    self.opening(&page(&alone), &alone, path, lines)
                })
            })
            .collect()
    }

    /// The Mini App link opening the changes since the commit `since`, a full hash, of
    /// the working tree holding `path`, which resolves in the session's directory.
    pub fn changes(self, since: &str, path: &str) -> Option<String> {
        self.link(&format!(
            "{}_{since}_{}",
            self.session,
            URL_SAFE_NO_PAD.encode(path)
        ))
    }

    /// The link to `page`, a page of `files`, at `path` and its `lines`.
    fn opening(
        self,
        page: &str,
        files: &[Linked],
        path: &str,
        lines: Option<Lines>,
    ) -> Option<String> {
        let file = files.iter().position(|linked| linked.path == path)?;
        let at = lines.map_or(0, |lines| {
            1 + files[file]
                .lines
                .iter()
                .position(|&linked| linked == lines)
                .expect("the lines of a link are among its file's")
        });
        self.link(&format!("{}_{file}_{at}_{page}", self.session))
    }

    fn link(self, parameter: &str) -> Option<String> {
        (parameter.len() <= PARAMETER_MAX).then(|| format!("{}?startapp={parameter}", self.app))
    }
}

/// The part of a link to `files` that every link to them shares.
fn page(files: &[Linked]) -> String {
    let listed: Vec<String> = files.iter().map(Linked::listed).collect();
    URL_SAFE_NO_PAD.encode(listed.join("\n"))
}

/// The file a markdown link's `destination` names, and the lines its fragment names.
fn file(destination: &str) -> Option<(&str, Option<Lines>)> {
    let (path, fragment) = destination.split_once('#').unwrap_or((destination, ""));
    (!path.is_empty() && !scheme(path)).then(|| (path, lines(fragment)))
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
fn lines(fragment: &str) -> Option<Lines> {
    let range = fragment.strip_prefix('L')?;
    let (from, to) = range.split_once('-').unwrap_or((range, range));
    let from: usize = from.parse().ok()?;
    let to: usize = to.strip_prefix('L').unwrap_or(to).parse().ok()?;
    (from > 0 && to >= from).then_some(Lines { from, to })
}

/// Lines of a file, from 1.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Lines {
    pub from: usize,
    pub to: usize,
}

/// A file a message links to, as the message wrote it, and the lines its links name.
#[derive(Clone, Debug, PartialEq)]
pub struct Linked {
    pub path: String,
    pub lines: Vec<Lines>,
}

impl Linked {
    /// The file as a link lists it: its path, then a fragment for each of its lines.
    fn listed(&self) -> String {
        let fragments = self.lines.iter().map(|Lines { from, to }| {
            if from == to {
                format!("#L{from}")
            } else {
                format!("#L{from}-L{to}")
            }
        });
        std::iter::once(self.path.clone())
            .chain(fragments)
            .collect()
    }

    /// A path holds no `#`, since a link's fragment starts at its first one.
    fn parse(listed: &str) -> Option<Self> {
        let mut parts = listed.split('#');
        let path = parts.next().filter(|path| !path.is_empty())?;
        Some(Self {
            path: path.to_owned(),
            lines: parts.map(lines).collect::<Option<_>>()?,
        })
    }
}

/// What a link opens in a session. It travels as `<session>_<file>_<lines>_<files in
/// base64url>` for the files a message links to and `<session>_<since>_<path in
/// base64url>` for changes, which keeps to the alphabet `startapp` allows, since a
/// session id holds no `_`. The files are listed a line each, and `<file>` counts the
/// link's own among them from 0 and `<lines>` its lines among that file's from 1, 0 for
/// none.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub session: String,
    pub opens: Opens,
}

#[derive(Debug, PartialEq)]
pub enum Opens {
    /// The files a message links to, and the link's own file and lines among them.
    Files {
        files: Vec<Linked>,
        file: usize,
        lines: Option<Lines>,
    },
    /// The changes since the commit `since` of the working tree holding `path`.
    Changes { since: String, path: String },
}

impl Target {
    /// The fields after the session tell a link to changes from one to files by the
    /// first, since a count of files has fewer digits than a commit hash.
    pub fn decode(parameter: &str) -> Option<Self> {
        let (session, rest) = parameter.split_once('_')?;
        let (first, rest) = rest
            .split_once('_')
            .map_or((rest, None), |(first, rest)| (first, Some(rest)));
        let opens = if changes::hash(first) {
            Opens::Changes {
                since: first.to_owned(),
                path: rest.map_or(Some(".".to_owned()), text)?,
            }
        } else {
            let (at, listed) = rest?.split_once('_')?;
            let files = text(listed)?
                .split('\n')
                .map(Linked::parse)
                .collect::<Option<Vec<_>>>()?;
            let file: usize = first.parse().ok()?;
            let at: usize = at.parse().ok()?;
            let lines = match at.checked_sub(1) {
                None => None,
                Some(index) => Some(*files.get(file)?.lines.get(index)?),
            };
            files.get(file)?;
            Opens::Files { files, file, lines }
        };
        Some(Self {
            session: session.to_owned(),
            opens,
        })
    }
}

/// The text `encoded` holds in base64url.
fn text(encoded: &str) -> Option<String> {
    String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0b7c4bd0-62d5-4b8e-9e05-2bd0f8f0a7d3";

    fn links() -> Links<'static> {
        Links::new(Some("https://t.me/bot/app"), SESSION).expect("links")
    }

    fn decoded(link: &str) -> Target {
        let parameter = link
            .strip_prefix("https://t.me/bot/app?startapp=")
            .expect("a startapp link");
        assert!(
            parameter
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "{parameter} keeps to the startapp alphabet"
        );
        Target::decode(parameter).expect("a link decodes")
    }

    /// What each of a message's links to `destinations` opens.
    fn opened(destinations: &[&str]) -> Vec<Option<Target>> {
        links()
            .files(destinations)
            .iter()
            .map(|link| link.as_deref().map(decoded))
            .collect()
    }

    fn linked(path: &str, lines: &[(usize, usize)]) -> Linked {
        Linked {
            path: path.to_owned(),
            lines: lines.iter().map(|&(from, to)| Lines { from, to }).collect(),
        }
    }

    fn target(files: &[Linked], file: usize, lines: Option<(usize, usize)>) -> Target {
        Target {
            session: SESSION.to_owned(),
            opens: Opens::Files {
                files: files.to_vec(),
                file,
                lines: lines.map(|(from, to)| Lines { from, to }),
            },
        }
    }

    #[test]
    fn a_link_to_a_file_opens_it_at_its_lines() {
        for (destination, path, lines) in [
            ("src/listen.rs#L588", "src/listen.rs", Some((588, 588))),
            ("src/listen.rs#L12-L20", "src/listen.rs", Some((12, 20))),
            ("/home/u/文档/a b.md", "/home/u/文档/a b.md", None),
            ("README.md#configuration", "README.md", None),
        ] {
            let files = [linked(path, lines.as_slice())];
            assert_eq!(opened(&[destination]), [Some(target(&files, 0, lines))]);
        }
    }

    #[test]
    fn the_links_of_a_message_open_every_file_it_links_to_at_their_own() {
        let files = [
            linked("src/a.rs", &[(3, 3), (10, 12)]),
            linked("README.md", &[]),
            linked("/w/b.rs", &[(7, 7)]),
        ];
        assert_eq!(
            opened(&[
                "src/a.rs#L3",
                "README.md",
                "https://example.com",
                "src/a.rs#L10-L12",
                "/w/b.rs#L7",
                "src/a.rs#L3",
            ]),
            [
                Some(target(&files, 0, Some((3, 3)))),
                Some(target(&files, 1, None)),
                None,
                Some(target(&files, 0, Some((10, 12)))),
                Some(target(&files, 2, Some((7, 7)))),
                Some(target(&files, 0, Some((3, 3)))),
            ]
        );
    }

    #[test]
    fn a_link_whose_page_is_too_long_for_telegram_opens_its_file_alone() {
        let long = "a/".repeat(170);
        let opened = opened(&[&long, "src/listen/b.rs#L2"]);
        assert_eq!(opened[0], Some(target(&[linked(&long, &[])], 0, None)));
        let short = [linked("src/listen/b.rs", &[(2, 2)])];
        assert_eq!(opened[1], Some(target(&short, 0, Some((2, 2)))));
    }

    #[test]
    fn a_link_to_changes_opens_them_since_its_commit() {
        let since = "0123456789abcdef0123456789abcdef01234567";
        let changes = |path: &str| Target {
            session: SESSION.to_owned(),
            opens: Opens::Changes {
                since: since.to_owned(),
                path: path.to_owned(),
            },
        };
        // The second path holds a `_` in base64url.
        for path in [".", "/home/u/??"] {
            let link = links().changes(since, path).expect("a link");
            let parameter = link
                .strip_prefix("https://t.me/bot/app?startapp=")
                .expect("a startapp link");
            assert_eq!(Target::decode(parameter), Some(changes(path)));
        }
        assert_eq!(
            Target::decode(&format!("{SESSION}_{since}")),
            Some(changes("."))
        );
    }

    #[test]
    fn a_link_elsewhere_stays_as_it_was() {
        let destinations = ["https://example.com/a.rs", "mailto:a@b.c", "#L3", ""];
        assert_eq!(links().files(&destinations), [None, None, None, None]);
    }

    #[test]
    fn a_link_too_long_for_telegram_stays_as_it_was() {
        assert!(links().files(&[&"a/".repeat(170)])[0].is_some());
        assert_eq!(links().files(&[&"a/".repeat(180)]), [None]);
    }

    #[test]
    fn a_link_naming_lines_or_a_file_past_its_list_names_nothing() {
        let listed = URL_SAFE_NO_PAD.encode("a.rs#L2\nb.rs");
        for at in ["0_0", "0_1", "1_0"] {
            assert!(
                Target::decode(&format!("{SESSION}_{at}_{listed}")).is_some(),
                "{at}"
            );
        }
        for at in ["0_2", "1_1", "2_0"] {
            assert_eq!(
                Target::decode(&format!("{SESSION}_{at}_{listed}")),
                None,
                "{at}"
            );
        }
    }

    #[test]
    fn a_session_outside_the_startapp_alphabet_has_no_links() {
        assert!(Links::new(Some("https://t.me/bot/app"), "a_b").is_none());
        assert!(Links::new(None, SESSION).is_none());
    }
}

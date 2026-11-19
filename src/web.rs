//! The links that open the files an answer names, or the changes of a turn, in the Telegram
//! Mini App the daemon serves. Both show a session's working tree since a commit, the files
//! a message links to or every changed one. A link carries what it opens in its `startapp`
//! parameter, and the daemon keeps the files of a message that links to more than the
//! parameter holds, in its state directory, so a link posted before a restart still opens.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest;
use serde::Serialize;

use crate::changes;
use crate::telegram::setting;

/// The longest `startapp` parameter measured to reach the page, since Telegram refused
/// one of 2048 characters.
const PARAMETER_MAX: usize = 1024;

/// Where the daemon serves the page, or `None` when the env file has it serve none.
pub fn listen() -> Option<SocketAddr> {
    setting("WEB_LISTEN").map(|listen| {
        listen
            .parse()
            .unwrap_or_else(|error| panic!("WEB_LISTEN {listen}: {error}"))
    })
}

/// The main Mini App of a bot, and where the daemon keeps the files of the messages
/// linking to it.
pub struct App {
    link: String,
    lists: Arc<Lists>,
}

impl App {
    /// The Mini App of the bot named `username`, a link to which hands the `startapp`
    /// parameter appended to it on to the page.
    pub fn new(username: &str, lists: Arc<Lists>) -> Self {
        Self {
            link: format!("https://t.me/{username}"),
            lists,
        }
    }

    pub fn lists(&self) -> &Lists {
        &self.lists
    }
}

/// The files of each message that links to more than a link carries, a file per list in
/// the directory of its session, under a name made of its content, so a message
/// rendered again keeps one list.
pub struct Lists {
    directory: PathBuf,
    /// Whether each list tried since the daemon started was written, so a list is
    /// written, or its failure logged, once.
    tried: Mutex<HashMap<PathBuf, bool>>,
}

impl Lists {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            tried: Mutex::default(),
        }
    }

    /// The name `listed` is kept under for `session`, `None` when it cannot be written.
    fn keep(&self, session: &str, listed: &str) -> Option<String> {
        let hash = digest::digest(&digest::SHA256, listed.as_bytes());
        let name = hash.as_ref()[..8]
            .iter()
            .fold(String::new(), |mut name, byte| {
                write!(name, "{byte:02x}").expect("a String takes writes");
                name
            });
        let directory = self.directory.join(session);
        let file = directory.join(&name);
        let mut tried = self.tried.lock().expect("no holder of the lock panics");
        let written = *tried.entry(file.clone()).or_insert_with(|| {
            // Renamed into place, so the page reads a list whole.
            let written = file.with_extension("tmp");
            fs::create_dir_all(&directory)
                .and_then(|()| fs::write(&written, listed))
                .and_then(|()| fs::rename(&written, &file))
                .inspect_err(|error| log::warn!("{}: {error}", file.display()))
                .is_ok()
        });
        written.then_some(name)
    }

    /// The list kept for `session` under `name`. Both come from a link, so only names
    /// that stay inside the directory are read.
    fn kept(&self, session: &str, name: &str) -> Option<String> {
        (plain(session) && plain(name))
            .then(|| fs::read_to_string(self.directory.join(session).join(name)).ok())
            .flatten()
    }

    /// Drops the lists of `session`, which the daemon has forgotten.
    pub fn forget(&self, session: &str) {
        if !plain(session) {
            return;
        }
        let directory = self.directory.join(session);
        self.tried
            .lock()
            .expect("no holder of the lock panics")
            .retain(|file, _| !file.starts_with(&directory));
        match fs::remove_dir_all(&directory) {
            Err(error) if error.kind() != ErrorKind::NotFound => {
                log::warn!("{}: {error}", directory.display());
            }
            _ => {}
        }
    }
}

/// Whether `name` survives the `startapp` alphabet and names one file of a directory,
/// as a session id Claude Code makes, a UUID, does.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// How the links of one session's answer are rewritten: the Mini App to link to, the
/// session a relative path is resolved in, and the commit its turn started from, which
/// the page shows the files' changes since.
#[derive(Clone)]
pub struct Links<'a> {
    app: &'a App,
    session: &'a str,
    since: Option<String>,
}

impl<'a> Links<'a> {
    /// `None` without a Mini App to link to, and for a session id that is not `plain`.
    /// `since` is a full commit hash.
    pub fn new(app: Option<&'a App>, session: &'a str, since: Option<String>) -> Option<Self> {
        let app = app?;
        plain(session).then_some(Self {
            app,
            session,
            since,
        })
    }

    /// The Mini App link of each of a message's markdown link `destinations`, all opening
    /// one page of every file the message links to, each at its own file and lines. A
    /// destination names a file when it has no scheme and is not a fragment of the page
    /// itself, and a fragment of `L12` or `L12-L20` names the lines. A link too long for
    /// Telegram with the files in it names the list the daemon keeps of them. When that
    /// list cannot be kept, the link opens its file alone, and stays as it was when even
    /// that is too long.
    pub fn files(&self, destinations: &[&str]) -> Vec<Option<String>> {
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
        let listed = listed(&files);
        let carried = Page::Carried(URL_SAFE_NO_PAD.encode(&listed));
        let kept = OnceCell::new();
        named
            .iter()
            .map(|named| {
                let (path, lines) = (*named)?;
                self.opening(&carried, &files, path, lines)
                    .or_else(|| {
                        let kept = kept
                            .get_or_init(|| self.app.lists.keep(self.session, &listed))
                            .clone()?;
                        self.opening(&Page::Kept(kept), &files, path, lines)
                    })
                    .or_else(|| {
                        let alone = [Linked {
                            path: path.to_owned(),
                            lines: lines.into_iter().collect(),
                        }];
                        let carried = Page::Carried(URL_SAFE_NO_PAD.encode(self::listed(&alone)));
                        self.opening(&carried, &alone, path, lines)
                    })
            })
            .collect()
    }

    /// The Mini App link opening the changes since the commit `since`, a full hash, of
    /// the working tree holding `path`, which resolves in the session's directory.
    pub fn changes(&self, since: &str, path: &str) -> Option<String> {
        self.link(&format!(
            "{}_{since}_{}",
            self.session,
            URL_SAFE_NO_PAD.encode(path)
        ))
    }

    /// The link to `page`, a page of `files`, at `path` and its `lines`.
    fn opening(
        &self,
        page: &Page,
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
        let (session, since) = (self.session, self.since.as_deref().unwrap_or_default());
        self.link(&match page {
            Page::Carried(listed) => format!("{session}_{file}_{at}_{since}_{listed}"),
            Page::Kept(name) => format!("{session}_{KEPT}{name}_{file}_{at}_{since}"),
        })
    }

    fn link(&self, parameter: &str) -> Option<String> {
        (parameter.len() <= PARAMETER_MAX)
            .then(|| format!("{}?startapp={parameter}", self.app.link))
    }
}

/// What opens the field of a link that names a list the daemon keeps. It is no digit of
/// a count of files or of a commit hash, which open that field of the other links.
const KEPT: char = 'k';

/// The files of a page as its links name them: in base64url inside each link, or by the
/// name the daemon keeps their list under.
enum Page {
    Carried(String),
    Kept(String),
}

/// `files` as a link lists them, a line each.
fn listed(files: &[Linked]) -> String {
    let listed: Vec<String> = files.iter().map(Linked::listed).collect();
    listed.join("\n")
}

/// The file a markdown link's `destination` names, and the lines its fragment names, or
/// else a suffix such as `:12` or `:12-20`, as Claude Code writes a place in a file.
fn file(destination: &str) -> Option<(&str, Option<Lines>)> {
    let (path, lines) = match destination.split_once('#') {
        Some((path, fragment)) => (path, lines(fragment)),
        None => destination
            .rsplit_once(':')
            .and_then(|(path, suffix)| Some((path, Some(range(suffix)?))))
            .unwrap_or((destination, None)),
    };
    (!path.is_empty() && !scheme(path)).then_some((path, lines))
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
    range(fragment.strip_prefix('L')?)
}

/// The lines `12`, `12-20` or `12-L20` names.
fn range(range: &str) -> Option<Lines> {
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

/// What a link opens in a session. It travels as `<session>_<file>_<lines>_<since>_<files
/// in base64url>` for the files a message links to, as `<session>_k<list>_<file>_<lines>_
/// <since>` when the daemon keeps the list of them, and as `<session>_<since>_<path in
/// base64url>` for changes, which keeps to the alphabet `startapp` allows, since a
/// session id holds no `_`. The files are listed a line each, and `<file>` counts the
/// link's own among them from 0 and `<lines>` its lines among that file's from 1, 0 for
/// none. `<since>` of files is empty when their session ran in no working tree.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub session: String,
    pub opens: Opens,
}

#[derive(Debug, PartialEq)]
pub enum Opens {
    /// The files a message links to, and the link's own file and lines among them, with
    /// their changes since the commit `since` of the session's working tree.
    Files {
        since: Option<String>,
        files: Vec<Linked>,
        file: usize,
        lines: Option<Lines>,
    },
    /// The changes since the commit `since` of the working tree holding `path`.
    Changes { since: String, path: String },
}

/// Why a link opens nothing.
const NAMELESS: &str = "the link names no file";

impl Target {
    /// The fields after the session tell the links apart by the first: a commit hash for
    /// changes, `KEPT` for a list among `lists`, and a count of files, which has fewer
    /// digits than a hash, for files the link carries.
    pub fn decode(parameter: &str, lists: &Lists) -> Result<Self, &'static str> {
        let (session, rest) = parameter.split_once('_').ok_or(NAMELESS)?;
        let (first, rest) = rest
            .split_once('_')
            .map_or((rest, None), |(first, rest)| (first, Some(rest)));
        let opens = if changes::hash(first) {
            Opens::Changes {
                since: first.to_owned(),
                path: rest.map_or(Some(".".to_owned()), text).ok_or(NAMELESS)?,
            }
        } else {
            let mut fields = rest.ok_or(NAMELESS)?.splitn(3, '_');
            let mut field = || fields.next().ok_or(NAMELESS);
            let (file, at, since, listed) = if let Some(name) = first.strip_prefix(KEPT) {
                let listed = lists
                    .kept(session, name)
                    .ok_or("the daemon no longer keeps the files of that message")?;
                (field()?, field()?, field()?, listed)
            } else {
                let (at, since, listed) = (field()?, field()?, field()?);
                (first, at, since, text(listed).ok_or(NAMELESS)?)
            };
            Self::files(file, at, since, &listed).ok_or(NAMELESS)?
        };
        Ok(Self {
            session: session.to_owned(),
            opens,
        })
    }

    /// The files `listed` a line each since the commit `since`, empty for none, with
    /// the link's own, the one numbered `file`, and its lines, numbered `at`.
    fn files(file: &str, at: &str, since: &str, listed: &str) -> Option<Opens> {
        let since = match since {
            "" => None,
            since if changes::hash(since) => Some(since.to_owned()),
            _ => return None,
        };
        let files = listed
            .split('\n')
            .map(Linked::parse)
            .collect::<Option<Vec<_>>>()?;
        let file: usize = file.parse().ok()?;
        let at: usize = at.parse().ok()?;
        let lines = match at.checked_sub(1) {
            None => None,
            Some(index) => Some(*files.get(file)?.lines.get(index)?),
        };
        files.get(file)?;
        Some(Opens::Files {
            since,
            files,
            file,
            lines,
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
    const SINCE: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A Mini App keeping its lists in a directory of its own.
    fn app() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().expect("a directory");
        let lists = Lists::new(directory.path().join("links"));
        (directory, App::new("bot", Arc::new(lists)))
    }

    /// A Mini App whose lists cannot be written, since a file stands in their place.
    fn listless() -> (tempfile::TempDir, App) {
        let (directory, app) = app();
        fs::write(directory.path().join("links"), "").expect("a file");
        (directory, app)
    }

    fn links(app: &App) -> Links<'_> {
        Links::new(Some(app), SESSION, Some(SINCE.to_owned())).expect("links")
    }

    fn parameter(link: &str) -> &str {
        let parameter = link
            .strip_prefix("https://t.me/bot?startapp=")
            .expect("a startapp link");
        assert!(
            parameter
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "{parameter} keeps to the startapp alphabet"
        );
        parameter
    }

    fn decoded(app: &App, link: &str) -> Target {
        Target::decode(parameter(link), app.lists()).expect("a link decodes")
    }

    /// What each of a message's links to `destinations` opens in `app`.
    fn opened_in(app: &App, destinations: &[&str]) -> Vec<Option<Target>> {
        links(app)
            .files(destinations)
            .iter()
            .map(|link| link.as_deref().map(|link| decoded(app, link)))
            .collect()
    }

    fn opened(destinations: &[&str]) -> Vec<Option<Target>> {
        opened_in(&app().1, destinations)
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
                since: Some(SINCE.to_owned()),
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
            ("src/listen.rs:588", "src/listen.rs", Some((588, 588))),
            ("README.md:12-20", "README.md", Some((12, 20))),
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
    fn a_message_linking_more_than_a_link_carries_keeps_its_files_with_the_daemon() {
        let long = "a/".repeat(350);
        let files = [linked(&long, &[]), linked("src/listen/b.rs", &[(2, 2)])];
        let (_directory, app) = app();
        let links = links(&app);
        let kept = links.files(&[&long, "src/listen/b.rs#L2"]);
        let [long_link, short_link] =
            [&kept[0], &kept[1]].map(|link| link.as_deref().expect("a link"));
        assert!(parameter(short_link).len() < 128, "{short_link}");
        assert_eq!(decoded(&app, long_link), target(&files, 0, None));
        assert_eq!(decoded(&app, short_link), target(&files, 1, Some((2, 2))));
        // A message rendered again names the same list.
        assert_eq!(links.files(&[&long, "src/listen/b.rs#L2"]), kept);
        assert_eq!(
            fs::read_dir(app.lists.directory.join(SESSION))
                .expect("the lists of the session")
                .count(),
            1
        );

        app.lists().forget(SESSION);
        assert_eq!(
            Target::decode(parameter(short_link), app.lists()),
            Err("the daemon no longer keeps the files of that message")
        );
        // A list is named by hex digits alone, so a link reads no file outside them.
        fs::write(app.lists.directory.join("outside"), "a.rs").expect("a file");
        assert_eq!(
            Target::decode(&format!("{SESSION}_k../outside_0_0_"), app.lists()),
            Err("the daemon no longer keeps the files of that message")
        );
    }

    #[test]
    fn a_link_whose_list_cannot_be_kept_opens_its_file_alone() {
        let long = "a/".repeat(350);
        let (_directory, app) = listless();
        let opened = opened_in(&app, &[&long, "src/listen/b.rs#L2"]);
        assert_eq!(opened[0], Some(target(&[linked(&long, &[])], 0, None)));
        let short = [linked("src/listen/b.rs", &[(2, 2)])];
        assert_eq!(opened[1], Some(target(&short, 0, Some((2, 2)))));
        // Even alone, a path may be too long for Telegram.
        assert_eq!(links(&app).files(&[&"a/".repeat(360)]), [None]);
    }

    #[test]
    fn a_link_of_a_session_outside_a_working_tree_opens_its_files_alone() {
        let (_directory, app) = app();
        let links = Links::new(Some(&app), SESSION, None).expect("links");
        let link = links.files(&["a.rs#L2"]).remove(0).expect("a link");
        let Opens::Files { since, files, .. } = decoded(&app, &link).opens else {
            panic!("{link} opens no files");
        };
        assert_eq!((since, files), (None, vec![linked("a.rs", &[(2, 2)])]));
    }

    #[test]
    fn a_link_to_changes_opens_them_since_its_commit() {
        let since = SINCE;
        let changes = |path: &str| Target {
            session: SESSION.to_owned(),
            opens: Opens::Changes {
                since: since.to_owned(),
                path: path.to_owned(),
            },
        };
        // The second path holds a `_` in base64url.
        let (_directory, app) = app();
        for path in [".", "/home/u/??"] {
            let link = links(&app).changes(since, path).expect("a link");
            assert_eq!(decoded(&app, &link), changes(path));
        }
        assert_eq!(
            Target::decode(&format!("{SESSION}_{since}"), app.lists()),
            Ok(changes("."))
        );
    }

    #[test]
    fn a_link_elsewhere_stays_as_it_was() {
        let destinations = [
            "https://example.com/a.rs",
            "https://example.com:8080",
            "mailto:a@b.c",
            "#L3",
            "",
        ];
        let (_directory, app) = app();
        assert_eq!(
            links(&app).files(&destinations),
            [None, None, None, None, None]
        );
    }

    #[test]
    fn a_link_naming_lines_or_a_file_past_its_list_or_no_commit_names_nothing() {
        let listed = URL_SAFE_NO_PAD.encode("a.rs#L2\nb.rs");
        let (_directory, app) = app();
        let decode = |at: &str| Target::decode(&format!("{SESSION}_{at}_{listed}"), app.lists());
        for at in ["0_0_", "0_1_", &format!("1_0_{SINCE}")] {
            assert!(decode(at).is_ok(), "{at}");
        }
        for at in ["0_2_", "1_1_", "2_0_", &format!("0_0_{}", &SINCE[..12])] {
            assert_eq!(decode(at), Err(NAMELESS), "{at}");
        }
    }

    #[test]
    fn a_session_outside_the_startapp_alphabet_has_no_links() {
        let (_directory, app) = app();
        assert!(Links::new(Some(&app), "a_b", None).is_none());
        assert!(Links::new(None, SESSION, None).is_none());
    }
}

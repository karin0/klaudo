//! The server of the Mini App a link in an answer opens: the page, the files or the
//! changes the link names, and the review the page sends back, which the session receives
//! as a prompt. It runs on a thread of its own, and asks the machine, which owns the
//! sessions, through the channel the other input threads feed.

use std::convert::Infallible;
use std::fmt::Write;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use kuriero::User;
use ring::digest;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::changes::{self, Commit};
use crate::hook;
use crate::telegram::{Sound, Telegram};
use crate::web::{self, Linked, Opens, Target};

use super::chat::{IN_DIALOG, unseen};
use super::turn::Ask;
use super::{Arrival, Machine, now_millis};

/// Each file of `web/dist` with its path under the page and its type, as `build.rs` lists
/// them.
const ASSETS: &[(&str, &str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/assets.rs"));
/// A request comes whole from the proxy in front, so one slower than this has stalled.
const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// Longer than a Telegram call retrying a rate limit can hold up the machine.
const MACHINE_WAIT: Duration = Duration::from_secs(100);
const REVIEW_MAX: usize = 1 << 20;
/// A phone shows a larger file too slowly to read it there.
const FILE_MAX: u64 = 1 << 20;
/// How long the `initData` Telegram hands the page when it opens stays valid, in seconds.
/// Every request of the page carries the `initData` it opened with, so a review left open
/// longer than this loses its comments.
const SIGNED_MAX: u64 = 6 * 60 * 60;
/// The Ed25519 key, in hex, Telegram signs the `initData` of every Mini App with.
const TELEGRAM_KEY: &str = "e7bf03a2fa4602af4580703d88dda5bb59f32ed8b02a56c187fe7d34caed242d";
const OUTSIDE: &str = "that conversation runs outside tmux, where nothing can be typed into it";

/// Who opened the page, and on which session.
#[derive(Clone)]
pub(super) struct Visit {
    session: String,
    /// The name of the guest who opened it, `None` for the user.
    guest: Option<String>,
}

/// What the page asks of the machine, with where its answer goes.
pub(super) enum Asked {
    /// The files `paths` name in the session's directory, which the machine resolves
    /// and checks against what the visitor may see, once the visitor may see the session.
    Files {
        visit: Visit,
        paths: Vec<String>,
        answer: oneshot::Sender<Result<Vec<Result<PathBuf, String>>, String>>,
    },
    /// The session's directory, and the working tree holding the directory `path` names
    /// in it.
    Tree {
        visit: Visit,
        path: String,
        answer: oneshot::Sender<Result<Tree, String>>,
    },
    /// A prompt to type into the session.
    Review {
        visit: Visit,
        prompt: String,
        answer: oneshot::Sender<Result<(), String>>,
    },
}

/// The directory a session runs in, and the top of a working tree, or the reason the page
/// cannot show its changes: there is none, or the visitor may not see all of it.
pub(super) struct Tree {
    dir: PathBuf,
    top: Result<PathBuf, String>,
}

type Refusal = (StatusCode, String);

/// What the page sends back: its comments, the commit the changes it showed start from,
/// which the lines it shows as removed belong to, and a message for the whole review.
#[derive(Deserialize)]
struct Review {
    since: Option<String>,
    notes: Vec<Note>,
    text: String,
}

/// One comment of a review, on the lines it spans in the file as it is, in the file as
/// of `since`, or in both.
#[derive(Deserialize)]
struct Note {
    new: Option<Lines>,
    old: Option<Lines>,
    /// The lines as a unified diff shows them, each behind ` `, `-` or `+`.
    lines: Vec<String>,
    text: String,
    /// What the lines of the file as it is should read instead.
    suggestion: Option<String>,
}

#[derive(Deserialize)]
struct Lines {
    path: String,
    from: usize,
    to: usize,
}

/// What a link opens, as the page draws it, with how many unchanged lines it shows around
/// a change or a line a link names.
#[derive(Serialize)]
struct Page {
    context: usize,
    #[serde(flatten)]
    opened: Opened,
}

/// A session's working tree since a commit: the files a message links to, with the link's
/// own file and lines among them, or every changed file. The files have no start when the
/// session runs in no working tree, or the visitor may not see all of it.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Opened {
    Files {
        start: Option<Start>,
        files: Vec<Shown>,
        file: usize,
        lines: Option<web::Lines>,
    },
    Changes {
        start: Start,
        /// A git patch, untracked files as new ones.
        patch: String,
    },
}

/// The commit the changes start from, and the commits the page moves it along.
#[derive(Serialize)]
struct Start {
    since: String,
    commits: Vec<Commit>,
}

/// A file a message links to, with the lines its links name, and its text or the reason
/// the visitor cannot read it. With a working tree to show the changes of, its path is
/// relative to the tree's top, or absolute outside it, and is as the link wrote it
/// otherwise.
#[derive(Serialize)]
struct Shown {
    path: String,
    lines: Vec<web::Lines>,
    #[serde(flatten)]
    read: Read,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Read {
    /// `old` is the file as of the start when it changed since then, empty when the start
    /// had none.
    Text {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old: Option<String>,
    },
    Refusal {
        refusal: String,
    },
}

/// A changed file as of the start and as it is, for the page to show the lines around a
/// change.
#[derive(Serialize)]
struct Contents {
    old: String,
    new: String,
}

struct Server {
    verifier: Verifier,
    telegram: Arc<Telegram>,
    landed: Sender<Arrival>,
    /// The entity tag of each of `ASSETS`, so a browser keeps a page it already has.
    tags: Vec<String>,
}

pub(super) fn serve(listener: TcpListener, telegram: Arc<Telegram>, landed: Sender<Arrival>) {
    let token = crate::telegram::setting("BOT_TOKEN").expect("the daemon has a BOT_TOKEN");
    let (bot, _) = token
        .split_once(':')
        .expect("BOT_TOKEN is the bot's id, a colon and its secret");
    let key = crate::telegram::setting("INIT_DATA_KEY");
    let server = Arc::new(Server {
        verifier: Verifier::new(bot, key.as_deref().unwrap_or(TELEGRAM_KEY)),
        telegram,
        landed,
        tags: ASSETS.iter().map(|(_, _, body)| tag(body)).collect(),
    });
    listener
        .set_nonblocking(true)
        .expect("a nonblocking listener");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener).expect("a listener");
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    eprintln!("web accept: {error}");
                    continue;
                }
            };
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let service = service_fn(|request| {
                    let server = Arc::clone(&server);
                    async move { Ok::<_, Infallible>(server.answer(request).await) }
                });
                // An idle connection the proxy keeps open times out between requests.
                if let Err(error) = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(REQUEST_WAIT)
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    && !error.is_timeout()
                {
                    eprintln!("web: {error}");
                }
            });
        }
    });
}

impl Server {
    async fn answer(&self, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let answered = match (request.method(), request.uri().path()) {
            (&Method::GET, "/api/open") => self.open(&request).await,
            (&Method::GET, "/api/contents") => self.contents(&request).await,
            (&Method::POST, "/api/review") => self.review(request).await,
            (&Method::GET, path) => self.asset(path, &request),
            _ => Err((StatusCode::NOT_FOUND, "no such page".to_owned())),
        };
        answered.unwrap_or_else(|(status, reason)| {
            reply(status, "text/plain; charset=utf-8", Bytes::from(reason))
        })
    }

    fn asset(
        &self,
        path: &str,
        request: &Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, Refusal> {
        let path = if path == "/" { "/index.html" } else { path };
        let page = path == "/index.html";
        let index = ASSETS
            .iter()
            .position(|(served, _, _)| *served == path)
            .ok_or((StatusCode::NOT_FOUND, "no such page".to_owned()))?;
        let (_, kind, body) = ASSETS[index];
        let tag = &self.tags[index];
        let kept = request
            .headers()
            .get(header::IF_NONE_MATCH)
            .is_some_and(|tags| tags.as_bytes() == tag.as_bytes());
        let mut response = if kept {
            reply(StatusCode::NOT_MODIFIED, kind, Bytes::new())
        } else {
            reply(StatusCode::OK, kind, Bytes::from_static(body))
        };
        let headers = response.headers_mut();
        headers.insert(header::ETAG, HeaderValue::from_str(tag).expect("a hex tag"));
        // A deploy replaces the page under the same name, and the page loads every other
        // file under a name carrying a hash of its content.
        let cache = if page {
            "no-cache"
        } else {
            "public, max-age=31536000, immutable"
        };
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        Ok(response)
    }

    /// What the link names. The page moves the start with `?since=`.
    async fn open(&self, request: &Request<Incoming>) -> Result<Response<Full<Bytes>>, Refusal> {
        let (visit, target) = self.visitor(request)?;
        let opened = match target.opens {
            Opens::Files {
                since,
                files,
                file,
                lines,
            } => {
                let since = parameter(request, "since").or(since);
                let Tree { top, .. } = self.tree(visit.clone(), ".".to_owned()).await?;
                let paths = files.iter().map(|linked| linked.path.clone()).collect();
                let found = self.files(visit, paths).await?;
                blocking(move || {
                    let top = top.ok();
                    let start = top
                        .as_deref()
                        .zip(since)
                        .map(|(top, since)| started(top, since))
                        .transpose()?;
                    let since = start.as_ref().map(|start| start.since.as_str());
                    let files = files
                        .into_iter()
                        .zip(found)
                        .map(|(linked, found)| shown(linked, found, top.as_deref(), since))
                        .collect::<Result<_, _>>()?;
                    Ok(Opened::Files {
                        start,
                        files,
                        file,
                        lines,
                    })
                })
                .await?
            }
            Opens::Changes { since, path } => {
                let since = parameter(request, "since").unwrap_or(since);
                let top = self.tree(visit, path).await?.top.map_err(conflict)?;
                blocking(move || {
                    let start = started(&top, since)?;
                    let patch = changes::patch(&top, &start.since)?;
                    Ok(Opened::Changes { start, patch })
                })
                .await?
            }
        };
        let page = Page {
            context: changes::CONTEXT,
            opened,
        };
        let body = serde_json::to_vec(&page).expect("what a link opens serializes");
        Ok(reply(StatusCode::OK, "application/json", Bytes::from(body)))
    }

    /// The file `?path=` names as it is, and as `?prev=` named it at `?since=`.
    async fn contents(
        &self,
        request: &Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, Refusal> {
        let (visit, target) = self.visitor(request)?;
        let Opens::Changes { path: held, .. } = target.opens else {
            return Err((
                StatusCode::BAD_REQUEST,
                "the link names no changes".to_owned(),
            ));
        };
        let missing = |name: &str| (StatusCode::BAD_REQUEST, format!("no {name} given"));
        let since = parameter(request, "since").ok_or_else(|| missing("since"))?;
        let path = parameter(request, "path").ok_or_else(|| missing("path"))?;
        let prev = parameter(request, "prev").ok_or_else(|| missing("prev"))?;
        let top = self
            .tree(visit.clone(), held)
            .await?
            .top
            .map_err(conflict)?;
        let file = self
            .files(visit, vec![top.join(&path).display().to_string()])
            .await?
            .pop()
            .expect("a file for the path asked")
            .map_err(|reason| (StatusCode::CONFLICT, reason))?;
        let new = read(&file, &path)?;
        let old = blocking(move || {
            changes::verify(&top, &since)?;
            changes::old(&top, &since, &prev)?.ok_or_else(|| format!("{since} has no {prev}"))
        })
        .await?;
        let old = was(old, &path)?;
        let body = serde_json::to_vec(&Contents { old, new }).expect("contents serialize");
        Ok(reply(StatusCode::OK, "application/json", Bytes::from(body)))
    }

    async fn files(
        &self,
        visit: Visit,
        paths: Vec<String>,
    ) -> Result<Vec<Result<PathBuf, String>>, Refusal> {
        self.ask(
            |answer| Asked::Files {
                visit,
                paths,
                answer,
            },
            "the daemon is busy; open the link again",
        )
        .await
    }

    async fn tree(&self, visit: Visit, path: String) -> Result<Tree, Refusal> {
        self.ask(
            |answer| Asked::Tree {
                visit,
                path,
                answer,
            },
            "the daemon is busy; open the link again",
        )
        .await
    }

    async fn review(&self, request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Refusal> {
        let (visit, target) = self.visitor(&request)?;
        let body = tokio::time::timeout(
            REQUEST_WAIT,
            Limited::new(request.into_body(), REVIEW_MAX).collect(),
        )
        .await
        .map_err(|_| {
            (
                StatusCode::REQUEST_TIMEOUT,
                "the review arrived too slowly".to_owned(),
            )
        })?
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?
        .to_bytes();
        let review: Review = serde_json::from_slice(&body)
            .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
        // A path the page shows changes of is relative to the top of the working tree,
        // and the prompt writes it as the session would.
        let held = match &target.opens {
            Opens::Files { .. } => ".".to_owned(),
            Opens::Changes { path, .. } => path.clone(),
        };
        let Tree { dir, top } = self.tree(visit.clone(), held).await?;
        let top = match target.opens {
            Opens::Files { .. } => top.ok(),
            Opens::Changes { .. } => Some(top.map_err(conflict)?),
        };
        let shown = |path: &str| match &top {
            None => path.to_owned(),
            Some(top) => {
                let file = top.join(path);
                file.strip_prefix(&dir)
                    .unwrap_or(&file)
                    .display()
                    .to_string()
            }
        };
        let prompt = prompt(&review, shown).map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        self.ask(
            |answer| Asked::Review {
                visit,
                prompt,
                answer,
            },
            "the daemon is busy, and the review may still arrive; check the chat before sending it again",
        )
        .await?;
        Ok(reply(StatusCode::NO_CONTENT, "text/plain", Bytes::new()))
    }

    /// Who opened the page and on what, from the `initData` Telegram signed when it
    /// opened, which the page passes on as `Authorization: tma <initData>`.
    fn visitor(&self, request: &Request<Incoming>) -> Result<(Visit, Target), Refusal> {
        let unauthorized = |reason: &str| (StatusCode::UNAUTHORIZED, reason.to_owned());
        let data = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("tma "))
            .ok_or_else(|| unauthorized("open this page from a link in the chat"))?;
        let signed = verify(&self.verifier, data, now_millis() / 1000).map_err(unauthorized)?;
        let guest = self
            .telegram
            .guest(&signed.user)
            .map_err(|reason| (StatusCode::FORBIDDEN, reason.to_owned()))?;
        let target = Target::decode(&signed.start)
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "the link names no file".to_owned()))?;
        let visit = Visit {
            session: target.session.clone(),
            guest,
        };
        Ok((visit, target))
    }

    /// The machine's answer to what `asked` makes of the channel it answers on, or
    /// `late` once it has taken too long.
    async fn ask<T>(
        &self,
        asked: impl FnOnce(oneshot::Sender<Result<T, String>>) -> Asked,
        late: &str,
    ) -> Result<T, Refusal> {
        let (answer, answered) = oneshot::channel();
        self.landed
            .send(Arrival::Page(asked(answer)))
            .expect("the machine is running");
        tokio::time::timeout(MACHINE_WAIT, answered)
            .await
            .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, late.to_owned()))?
            .expect("the machine answers every request")
            .map_err(|reason| (StatusCode::CONFLICT, reason))
    }
}

fn conflict(reason: String) -> Refusal {
    (StatusCode::CONFLICT, reason)
}

/// The start at `since` of the working tree at `top`, once `since` passes `verify`.
fn started(top: &Path, since: String) -> Result<Start, String> {
    changes::verify(top, &since)?;
    Ok(Start {
        commits: changes::commits(top, &since)?,
        since,
    })
}

/// How the page shows the file a message links to as `linked`, which resolved to `found`:
/// with its changes since `since` when the working tree at `top` holds it. A failing git
/// command fails the page, and a file the visitor cannot read is refused alone.
fn shown(
    linked: Linked,
    found: Result<PathBuf, String>,
    top: Option<&Path>,
    since: Option<&str>,
) -> Result<Shown, String> {
    let Linked { path, lines } = linked;
    let refused = |path: String, lines, refusal| Shown {
        path,
        lines,
        read: Read::Refusal { refusal },
    };
    let file = match found {
        Ok(file) => file,
        Err(refusal) => return Ok(refused(path, lines, refusal)),
    };
    let held = top.and_then(|top| Some((top, file.strip_prefix(top).ok()?.to_str()?)));
    let content = match read(&file, &path) {
        Ok(content) => content,
        Err((_, refusal)) => return Ok(refused(path, lines, refusal)),
    };
    let old = match held.zip(since) {
        None => None,
        Some(((top, inside), since)) => match changes::old(top, since, inside)? {
            None => changes::seen(top, inside)?.then(String::new),
            Some(old) => match was(old, &path) {
                Ok(old) => Some(old).filter(|old| *old != content),
                Err((_, refusal)) => return Ok(refused(path, lines, refusal)),
            },
        },
    };
    let path = match (top, held) {
        (None, _) => path,
        (Some(_), Some((_, inside))) => inside.to_owned(),
        (Some(_), None) => file.display().to_string(),
    };
    Ok(Shown {
        path,
        lines,
        read: Read::Text { content, old },
    })
}

/// The query parameter `name` of `request`.
fn parameter(request: &Request<Incoming>, name: &str) -> Option<String> {
    form_urlencoded::parse(request.uri().query()?.as_bytes())
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.into_owned())
}

/// What `work`, which runs git, makes, off the thread serving every request.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, Refusal> {
    tokio::task::spawn_blocking(work)
        .await
        .expect("git work runs to its end")
        .map_err(|reason| (StatusCode::UNPROCESSABLE_ENTITY, reason))
}

fn reply(status: StatusCode, kind: &'static str, body: Bytes) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    response
}

/// The text of the file at `file`, which the link wrote as `path`.
fn read(file: &Path, path: &str) -> Result<String, Refusal> {
    let unreadable = |error: std::io::Error| (StatusCode::NOT_FOUND, format!("{path}: {error}"));
    let metadata = fs::metadata(file).map_err(unreadable)?;
    if !metadata.is_file() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{path} is not a file"),
        ));
    }
    if metadata.len() > FILE_MAX {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("{path} is larger than 1 MiB"),
        ));
    }
    String::from_utf8(fs::read(file).map_err(unreadable)?).map_err(|_| {
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("{path} is not text in UTF-8"),
        )
    })
}

/// The text of `old`, a version of the file the link wrote as `path`.
fn was(old: Vec<u8>, path: &str) -> Result<String, Refusal> {
    if old.len() as u64 > FILE_MAX {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("{path} was larger than 1 MiB"),
        ));
    }
    String::from_utf8(old).map_err(|_| {
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("{path} was no text in UTF-8"),
        )
    })
}

fn tag(body: &[u8]) -> String {
    format!(
        "\"{}\"",
        hex(&digest::digest(&digest::SHA256, body).as_ref()[..12])
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, byte| {
        write!(hex, "{byte:02x}").expect("a string takes any write");
        hex
    })
}

/// What a valid `initData` vouches for: who opened the page, and the `startapp`
/// parameter of the link they opened it from.
#[derive(Debug)]
struct Signed {
    user: User,
    start: String,
}

/// What checks the `initData` of the bot's Mini Apps. Telegram signs it twice, and only
/// the Ed25519 signature is checked, since the HMAC beside it is keyed by the bot token,
/// which would let whoever holds the token open the page as anyone.
struct Verifier {
    /// The bot's id, which opens the text Telegram signs.
    bot: String,
    key: UnparsedPublicKey<Vec<u8>>,
}

impl Verifier {
    /// The verifier of the bot with the id `bot`, whose `initData` Telegram signs with the
    /// Ed25519 key `key` in hex.
    fn new(bot: &str, key: &str) -> Self {
        let key = unhex(key)
            .filter(|key| key.len() == 32)
            .expect("INIT_DATA_KEY is an Ed25519 key in hex");
        Self {
            bot: bot.to_owned(),
            key: UnparsedPublicKey::new(&ED25519, key),
        }
    }
}

/// The fields of `data`, once its `signature` proves Telegram signed them for the bot of
/// `verifier` within `SIGNED_MAX` of `now`, in Unix seconds. The signature covers the line
/// `<bot id>:WebAppData`, then every field other than `hash` and itself, sorted by name,
/// each as `name=value` on a line of its own.
fn verify(verifier: &Verifier, data: &str, now: u64) -> Result<Signed, &'static str> {
    let mut signature = None;
    let mut fields: Vec<(String, String)> = Vec::new();
    for (name, value) in form_urlencoded::parse(data.as_bytes()) {
        match &*name {
            "signature" => signature = Some(value.into_owned()),
            "hash" => {}
            _ => fields.push((name.into_owned(), value.into_owned())),
        }
    }
    fields.sort();
    let checked = std::iter::once(format!("{}:WebAppData", verifier.bot))
        .chain(fields.iter().map(|(name, value)| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join("\n");
    let signature = signature
        .and_then(|signature| URL_SAFE_NO_PAD.decode(signature).ok())
        .ok_or("the page carries no signature")?;
    verifier
        .key
        .verify(checked.as_bytes(), &signature)
        .map_err(|_| "the page carries a forged signature")?;
    let field = |name: &str| {
        fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    };
    let signed_at: u64 = field("auth_date")
        .and_then(|date| date.parse().ok())
        .ok_or("the page carries no date")?;
    if now.saturating_sub(signed_at) > SIGNED_MAX {
        return Err("the page was opened over six hours ago; open the link again");
    }
    let user = field("user")
        .and_then(|user| serde_json::from_str(user).ok())
        .ok_or("the page names no user")?;
    let start = field("start_param")
        .ok_or("the page was opened from no link")?
        .to_owned();
    Ok(Signed { user, start })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    text.len().is_multiple_of(2).then_some(())?;
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// The prompt a review is typed into the session as: its message, or `Review:` without
/// one, then a block per note, each opening with
/// where it points, then the lines it suggests in a diff, or else the lines it comments
/// on, quoted when they are all in the file as it is, then what the reviewer wrote.
/// `shown` writes a path of the file as it is as the session would. Removed lines are
/// written as git names a file as of a commit, so Claude does not read them as lines of
/// the file as it is.
fn prompt(review: &Review, shown: impl Fn(&str) -> String) -> Result<String, String> {
    if review.notes.is_empty() {
        return Err("the review holds no comment".to_owned());
    }
    let text = review.text.trim();
    let mut blocks = vec![if text.is_empty() { "Review:" } else { text }.to_owned()];
    for note in &review.notes {
        let Note {
            new,
            old,
            lines,
            text,
            suggestion,
        } = note;
        let since = || match &review.since {
            Some(since) if changes::hash(since) => Ok(&since[..12]),
            _ => Err("removed lines need the commit they were removed since".to_owned()),
        };
        let place = match (new, old) {
            (Some(new), None) => new.at(&shown(&new.path))?,
            (None, Some(old)) => old.at(&format!("{}:{}", since()?, old.path))?,
            (Some(new), Some(old)) => format!(
                "{} (was {})",
                new.at(&shown(&new.path))?,
                old.at(&format!("{}:{}", since()?, old.path))?
            ),
            (None, None) => return Err("a comment spans no lines".to_owned()),
        };
        let text = text.trim();
        if text.is_empty() && suggestion.is_none() {
            return Err(format!("{place} has an empty comment"));
        }
        let rows = lines
            .iter()
            .map(|line| match line.split_at_checked(1) {
                Some((" " | "-" | "+", text)) => Ok((line.as_str(), text)),
                _ => Err(format!("{place}: {line:?} is no line of a diff")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut block = vec![place];
        match suggestion {
            Some(_) if old.is_some() => {
                return Err("a suggestion rewrites lines of the file as it is".to_owned());
            }
            Some(suggestion) => {
                let original = rows.iter().map(|(_, text)| *text).collect::<Vec<_>>();
                let fence = fence(&[&original.join("\n"), suggestion]);
                block.push(format!("{fence}diff"));
                block.extend(original.iter().map(|line| format!("-{line}")));
                // An empty suggestion deletes the lines.
                block.extend(suggestion.lines().map(|line| format!("+{line}")));
                block.push(fence);
            }
            None if old.is_none() => {
                block.extend(rows.iter().map(|(_, text)| format!("> {text}")));
            }
            None => {
                let fence = fence(&[&lines.join("\n")]);
                block.push(format!("{fence}diff"));
                block.extend(rows.iter().map(|(line, _)| (*line).to_owned()));
                block.push(fence);
            }
        }
        if !text.is_empty() {
            block.push(text.to_owned());
        }
        blocks.push(block.join("\n"));
    }
    Ok(blocks.join("\n\n"))
}

impl Lines {
    /// `file` followed by the lines, as in `src/main.rs:3-5`.
    fn at(&self, file: &str) -> Result<String, String> {
        let Self { from, to, .. } = *self;
        if from == 0 || to < from {
            return Err(format!("{file}: {from} to {to} is no range of lines"));
        }
        Ok(if from == to {
            format!("{file}:{from}")
        } else {
            format!("{file}:{from}-{to}")
        })
    }
}

/// A code fence longer than every run of backticks in `texts`, so none of them closes it.
fn fence(texts: &[&str]) -> String {
    let longest = texts
        .iter()
        .flat_map(|text| text.split(|c| c != '`'))
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

impl Machine {
    pub(super) fn page(&mut self, asked: Asked) {
        let refused = match asked {
            Asked::Files {
                visit,
                paths,
                answer,
            } => {
                let found = self.visited(&visit).map(|dir| {
                    paths
                        .iter()
                        .map(|path| self.resolved(&visit, &dir, path))
                        .collect()
                });
                answer.send(found).is_err()
            }
            Asked::Tree {
                visit,
                path,
                answer,
            } => answer.send(self.tree(&visit, &path)).is_err(),
            Asked::Review {
                visit,
                prompt,
                answer,
            } => answer.send(self.reviewed(visit, prompt)).is_err(),
        };
        if refused {
            eprintln!("web: the page stopped waiting for its answer");
        }
    }

    /// The directory of the session the page was opened on, once the visitor may see it:
    /// a guest only what lies in `CHAT_PROJECTS`.
    fn visited(&self, visit: &Visit) -> Result<PathBuf, String> {
        let dir = self
            .known()
            .find(|(id, _, _)| *id == visit.session)
            .map(|(_, dir, _)| dir.to_owned())
            .ok_or_else(|| unseen(&visit.session))?;
        self.admits(visit.guest.as_deref(), &dir)?;
        Ok(dir)
    }

    /// The file `path` names in the session's directory `dir`, once the visitor may see
    /// it, with every link resolved.
    fn resolved(&self, visit: &Visit, dir: &Path, path: &str) -> Result<PathBuf, String> {
        let file = dir
            .join(path)
            .canonicalize()
            .map_err(|error| format!("{path}: {error}"))?;
        self.admits(visit.guest.as_deref(), &file)?;
        Ok(file)
    }

    /// The session's directory, and the working tree holding the directory `path` names
    /// in it, whose changes the page shows only to a visitor who may see all of it.
    fn tree(&self, visit: &Visit, path: &str) -> Result<Tree, String> {
        let dir = self.visited(visit)?;
        let held = self.resolved(visit, &dir, path)?;
        let top = changes::top(&held)
            .and_then(|top| self.admits(visit.guest.as_deref(), &top).map(|()| top));
        Ok(Tree { dir, top })
    }

    /// Types a review into its session as a reply from the chat is typed. The review is
    /// posted first, quoted in the session's place, so the turn it starts replies to it,
    /// and so does a reply that resumes an exited session.
    fn reviewed(&mut self, visit: Visit, prompt: String) -> Result<(), String> {
        let id = visit.session;
        let (dir, place) = if let Some(session) = self.sessions.get(&id) {
            (session.dir.clone(), session.home(&self.telegram))
        } else {
            let ended = self
                .ended
                .iter()
                .find(|ended| ended.id == id)
                .ok_or_else(|| unseen(&id))?;
            let chat = self.telegram.chat(&ended.dir);
            (ended.dir.clone(), ended.trail.home(chat))
        };
        self.admits(visit.guest.as_deref(), &dir)?;
        // The review is refused before it is posted.
        if let Some(session) = self.sessions.get(&id) {
            if session.pane.is_none() {
                return Err(OUTSIDE.to_owned());
            }
            if session.turn.as_ref().is_some_and(|turn| turn.dialog) {
                return Err(IN_DIALOG.to_owned());
            }
        }
        let head = hook::Head::new(&dir, &id, None);
        let post = hook::compose(&head, "", "", &hook::quote(&prompt));
        let message = self
            .telegram
            .post(place, &post, Sound::Silent, None)
            .ok_or("Telegram refused the review")?;
        let ask = Ask {
            text: prompt,
            files: Vec::new(),
            place,
            message,
            guest: visit.guest,
        };
        self.send(&id, ask)
            .inspect_err(|_| self.telegram.delete(place.chat, message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// The key Telegram would sign with, were its private half this test's.
    fn pair() -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&[7; 32]).expect("a key from any seed")
    }

    fn verifier(bot: &str) -> Verifier {
        Verifier::new(bot, &hex(pair().public_key().as_ref()))
    }

    /// `fields` signed as Telegram signs them for the bot `111111`, beside a `hash` that
    /// only the bot token could check.
    fn signed(fields: &[(&str, &str)]) -> String {
        let mut sorted: Vec<String> = fields
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        sorted.sort();
        sorted.insert(0, "111111:WebAppData".to_owned());
        let signature = URL_SAFE_NO_PAD.encode(pair().sign(sorted.join("\n").as_bytes()));
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .append_pair("hash", &"0".repeat(64))
            .append_pair("signature", &signature)
            .finish()
    }

    const NOW: u64 = 1_800_000_000;

    fn fields(date: &str) -> Vec<(&'static str, String)> {
        vec![
            ("query_id", "AAE".to_owned()),
            (
                "user",
                r#"{"id":7,"first_name":"Ann","language_code":"en"}"#.to_owned(),
            ),
            ("auth_date", date.to_owned()),
            ("start_param", "s_1_2_YS5ycw".to_owned()),
        ]
    }

    fn data(fields: &[(&'static str, String)]) -> String {
        let borrowed: Vec<(&str, &str)> = fields
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        signed(&borrowed)
    }

    #[test]
    fn init_data_telegram_signed_names_its_user_and_link() {
        let signed = verify(&verifier("111111"), &data(&fields(&NOW.to_string())), NOW);
        let signed = signed.expect("valid");
        assert_eq!(signed.user.id, 7);
        assert_eq!(signed.start, "s_1_2_YS5ycw");
    }

    /// `initData` Telegram signed in production, as the tma.js documentation quotes it.
    #[test]
    fn init_data_telegram_signed_in_production_passes_the_signature_check() {
        let data = "user=%7B%22id%22%3A279058397%2C%22first_name%22%3A%22Vladislav%20%2B%20-%20%3F%20%5C%2F%22%2C%22last_name%22%3A%22Kibenko%22%2C%22username%22%3A%22vdkfrost%22%2C%22language_code%22%3A%22ru%22%2C%22is_premium%22%3Atrue%2C%22allows_write_to_pm%22%3Atrue%2C%22photo_url%22%3A%22https%3A%5C%2F%5C%2Ft.me%5C%2Fi%5C%2Fuserpic%5C%2F320%5C%2F4FPEE4tmP3ATHa57u6MqTDih13LTOiMoKoLDRG4PnSA.svg%22%7D\
            &chat_instance=8134722200314281151&chat_type=private&auth_date=1733584787\
            &signature=zL-ucjNyREiHDE8aihFwpfR9aggP2xiAo3NSpfe-p7IbCisNlDKlo7Kb6G4D0Ao2mBrSgEk4maLSdv6MLIlADQ\
            &hash=2174df5b000556d044f3f020384e879c8efcab55ddea2ced4eb752e93e7080d6";
        let verifier = Verifier::new("7342037359", TELEGRAM_KEY);
        // The sample was opened from no link.
        assert_eq!(
            verify(&verifier, data, 1_733_584_787).unwrap_err(),
            "the page was opened from no link"
        );
        let altered = data.replace("chat_type=private", "chat_type=group");
        assert_eq!(
            verify(&verifier, &altered, 1_733_584_787).unwrap_err(),
            "the page carries a forged signature"
        );
    }

    #[test]
    fn init_data_altered_or_signed_for_another_bot_is_refused() {
        let valid = data(&fields(&NOW.to_string()));
        let altered = valid.replace("s_1_2_YS5ycw", "s_1_2_Yi5ycw");
        assert_eq!(
            verify(&verifier("111111"), &altered, NOW).unwrap_err(),
            "the page carries a forged signature"
        );
        assert!(verify(&verifier("222222"), &valid, NOW).is_err());
        let other = Verifier::new("111111", TELEGRAM_KEY);
        assert!(verify(&other, &valid, NOW).is_err());
        let unsigned = valid
            .split("&signature=")
            .next()
            .expect("fields")
            .to_owned();
        assert_eq!(
            verify(&verifier("111111"), &unsigned, NOW).unwrap_err(),
            "the page carries no signature"
        );
    }

    #[test]
    fn init_data_older_than_six_hours_is_refused() {
        let oldest = NOW - SIGNED_MAX;
        let verifier = verifier("111111");
        assert!(verify(&verifier, &data(&fields(&oldest.to_string())), NOW).is_ok());
        assert_eq!(
            verify(&verifier, &data(&fields(&(oldest - 1).to_string())), NOW).unwrap_err(),
            "the page was opened over six hours ago; open the link again"
        );
    }

    fn lines(path: &str, from: usize, to: usize) -> Lines {
        Lines {
            path: path.to_owned(),
            from,
            to,
        }
    }

    fn note(from: usize, to: usize, original: &str, text: &str, suggestion: Option<&str>) -> Note {
        Note {
            new: Some(lines("src/telegram.rs", from, to)),
            old: None,
            lines: original
                .split('\n')
                .map(|line| format!(" {line}"))
                .collect(),
            text: text.to_owned(),
            suggestion: suggestion.map(str::to_owned),
        }
    }

    fn review(notes: Vec<Note>) -> Review {
        Review {
            since: None,
            notes,
            text: String::new(),
        }
    }

    #[test]
    fn a_review_reads_as_a_block_per_note() {
        let notes = vec![
            note(
                622,
                622,
                "    let mut line_start = false;",
                " clearer name\n",
                Some("    let mut at_line_start = false;"),
            ),
            note(
                640,
                641,
                "    written.push_str(&markdown[copied..]);\n    written",
                "这里为什么不直接返回？",
                None,
            ),
            Note {
                new: Some(lines("README.md", 3, 3)),
                ..note(3, 3, "```sh", "", Some(""))
            },
        ];
        assert_eq!(
            prompt(&review(notes), str::to_owned).expect("a prompt"),
            "Review:\n\
             \n\
             src/telegram.rs:622\n\
             ```diff\n\
             -    let mut line_start = false;\n\
             +    let mut at_line_start = false;\n\
             ```\n\
             clearer name\n\
             \n\
             src/telegram.rs:640-641\n\
             >     written.push_str(&markdown[copied..]);\n\
             >     written\n\
             这里为什么不直接返回？\n\
             \n\
             README.md:3\n\
             ````diff\n\
             -```sh\n\
             ````"
        );
    }

    const SINCE: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn removed_lines_are_named_as_of_the_start() {
        let replaced = Note {
            new: Some(lines("src/b.rs", 4, 4)),
            old: Some(lines("src/a.rs", 3, 4)),
            lines: vec![" x".to_owned(), "-y".to_owned(), "+z".to_owned()],
            text: "why".to_owned(),
            suggestion: None,
        };
        let removed = Note {
            new: None,
            old: Some(lines("src/a.rs", 9, 9)),
            lines: vec!["-w".to_owned()],
            ..note(0, 0, "", "gone?", None)
        };
        let review = Review {
            since: Some(SINCE.to_owned()),
            notes: vec![replaced, removed],
            text: String::new(),
        };
        assert_eq!(
            prompt(&review, |path| format!("sub/{path}")).expect("a prompt"),
            "Review:\n\
             \n\
             sub/src/b.rs:4 (was 0123456789ab:src/a.rs:3-4)\n\
             ```diff\n\
             \x20x\n\
             -y\n\
             +z\n\
             ```\n\
             why\n\
             \n\
             0123456789ab:src/a.rs:9\n\
             ```diff\n\
             -w\n\
             ```\n\
             gone?"
        );
    }

    #[test]
    fn a_message_opens_the_review_in_place_of_its_heading() {
        let review = Review {
            text: " fix these, then run the tests\n".to_owned(),
            ..review(vec![note(3, 3, "x", "why", None)])
        };
        assert_eq!(
            prompt(&review, str::to_owned).expect("a prompt"),
            "fix these, then run the tests\n\nsrc/telegram.rs:3\n> x\nwhy"
        );
    }

    #[test]
    fn a_review_without_a_comment_is_refused() {
        assert!(prompt(&review(vec![]), str::to_owned).is_err());
        let message = Review {
            text: "looks good".to_owned(),
            ..review(vec![])
        };
        assert!(prompt(&message, str::to_owned).is_err());
        for note in [
            note(3, 3, "x", " ", None),
            note(3, 2, "x", "why", None),
            note(0, 0, "x", "why", None),
            Note {
                new: None,
                ..note(3, 3, "x", "why", None)
            },
        ] {
            assert!(prompt(&review(vec![note]), str::to_owned).is_err());
        }
    }

    #[test]
    fn removed_lines_need_their_start_and_take_no_suggestion() {
        let removed = || Note {
            new: None,
            old: Some(lines("a.rs", 2, 2)),
            lines: vec!["-x".to_owned()],
            ..note(0, 0, "", "why", None)
        };
        assert!(prompt(&review(vec![removed()]), str::to_owned).is_err());
        let suggested = Review {
            since: Some(SINCE.to_owned()),
            notes: vec![Note {
                suggestion: Some("y".to_owned()),
                ..removed()
            }],
            text: String::new(),
        };
        assert!(prompt(&suggested, str::to_owned).is_err());
        let unmarked = Review {
            since: Some(SINCE.to_owned()),
            notes: vec![Note {
                lines: vec!["x".to_owned()],
                ..removed()
            }],
            text: String::new(),
        };
        assert!(prompt(&unmarked, str::to_owned).is_err());
    }
}

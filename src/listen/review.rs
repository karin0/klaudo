//! The server of the Mini App a file link in an answer opens: the page, the file the
//! link names, and the review the page sends back, which the session receives as a
//! prompt. It runs on a thread of its own, and asks the machine, which owns the sessions,
//! through the channel the other input threads feed.

use std::convert::Infallible;
use std::fmt::Write;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use kuriero::User;
use ring::{digest, hmac};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::hook;
use crate::telegram::{Sound, Telegram};
use crate::web::Target;

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
const SIGNED_MAX: u64 = 24 * 60 * 60;
const OUTSIDE: &str = "that conversation runs outside tmux, where nothing can be typed into it";

/// Who opened the page, and on which session.
pub(super) struct Visit {
    session: String,
    /// The name of the guest who opened it, `None` for the user.
    guest: Option<String>,
}

/// What the page asks of the machine, with where its answer goes.
pub(super) enum Asked {
    /// The file `path` names in the session's directory, which the machine resolves
    /// and checks against what the visitor may see.
    File {
        visit: Visit,
        path: String,
        answer: oneshot::Sender<Result<PathBuf, String>>,
    },
    /// A prompt to type into the session.
    Review {
        visit: Visit,
        prompt: String,
        answer: oneshot::Sender<Result<(), String>>,
    },
}

type Refusal = (StatusCode, String);

/// One comment of a review, on lines `from` to `to` of `path` as the page showed them.
#[derive(Deserialize)]
struct Note {
    path: String,
    from: usize,
    to: usize,
    original: String,
    text: String,
    /// What the lines should read instead.
    suggestion: Option<String>,
}

#[derive(Serialize)]
struct Shown<'a> {
    path: &'a str,
    content: &'a str,
    from: usize,
    to: usize,
}

struct Server {
    /// The key Telegram signs the `initData` of this bot's Mini Apps with.
    key: hmac::Key,
    telegram: Arc<Telegram>,
    landed: Sender<Arrival>,
    /// The entity tag of each of `ASSETS`, so a browser keeps a page it already has.
    tags: Vec<String>,
}

pub(super) fn serve(listener: TcpListener, telegram: Arc<Telegram>, landed: Sender<Arrival>) {
    let token = crate::telegram::setting("BOT_TOKEN").expect("the daemon has a BOT_TOKEN");
    let server = Arc::new(Server {
        key: key(&token),
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
            (&Method::GET, "/api/file") => self.file(&request).await,
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

    async fn file(&self, request: &Request<Incoming>) -> Result<Response<Full<Bytes>>, Refusal> {
        let (visit, target) = self.visitor(request)?;
        let path = target.path.clone();
        let file = self
            .ask(
                |answer| Asked::File {
                    visit,
                    path,
                    answer,
                },
                "the daemon is busy; open the link again",
            )
            .await?;
        let content = read(&file, &target.path)?;
        let shown = Shown {
            path: &target.path,
            content: &content,
            from: target.from,
            to: target.to,
        };
        let body = serde_json::to_vec(&shown).expect("a file serializes");
        Ok(reply(StatusCode::OK, "application/json", Bytes::from(body)))
    }

    async fn review(&self, request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Refusal> {
        let (visit, _) = self.visitor(&request)?;
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
        let notes: Vec<Note> = serde_json::from_slice(&body)
            .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
        let prompt = prompt(&notes).map_err(|error| (StatusCode::BAD_REQUEST, error))?;
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
        let signed = verify(&self.key, data, now_millis() / 1000).map_err(unauthorized)?;
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

/// The key Telegram signs a Mini App's `initData` with: HMAC-SHA256 of the bot token
/// under the key `WebAppData`.
fn key(token: &str) -> hmac::Key {
    let derived = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, b"WebAppData"),
        token.as_bytes(),
    );
    hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref())
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

/// The fields of `data`, once its `hash` proves Telegram signed them with `key` within
/// `SIGNED_MAX` of `now`, in Unix seconds. The signature covers every other field, sorted
/// by name, each as `name=value` on a line of its own.
fn verify(key: &hmac::Key, data: &str, now: u64) -> Result<Signed, &'static str> {
    let mut hash = None;
    let mut fields: Vec<(String, String)> = Vec::new();
    for (name, value) in form_urlencoded::parse(data.as_bytes()) {
        if name == "hash" {
            hash = Some(value.into_owned());
        } else {
            fields.push((name.into_owned(), value.into_owned()));
        }
    }
    fields.sort();
    let checked = fields
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let hash = hash
        .and_then(|hash| unhex(&hash))
        .ok_or("the page carries no signature")?;
    hmac::verify(key, checked.as_bytes(), &hash)
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
        return Err("the page was opened over a day ago; open the link again");
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

/// The prompt a review is typed into the session as: a block per note, each opening with
/// where it points, then the lines it suggests in a diff or the lines it comments on
/// quoted, then what the reviewer wrote.
fn prompt(notes: &[Note]) -> Result<String, String> {
    if notes.is_empty() {
        return Err("the review holds no comment".to_owned());
    }
    let mut blocks = vec!["Review:".to_owned()];
    for note in notes {
        let Note {
            path,
            from,
            to,
            original,
            text,
            suggestion,
        } = note;
        if *from == 0 || to < from {
            return Err(format!("{path}: {from} to {to} is no range of lines"));
        }
        let text = text.trim();
        if text.is_empty() && suggestion.is_none() {
            return Err(format!("{path}:{from} has an empty comment"));
        }
        let lines = if from == to {
            from.to_string()
        } else {
            format!("{from}-{to}")
        };
        let mut block = vec![format!("{path}:{lines}")];
        match suggestion {
            Some(suggestion) => {
                let fence = fence(&[original, suggestion]);
                block.push(format!("{fence}diff"));
                block.extend(original.split('\n').map(|line| format!("-{line}")));
                // An empty suggestion deletes the lines.
                block.extend(suggestion.lines().map(|line| format!("+{line}")));
                block.push(fence);
            }
            None => block.extend(original.split('\n').map(|line| format!("> {line}"))),
        }
        if !text.is_empty() {
            block.push(text.to_owned());
        }
        blocks.push(block.join("\n"));
    }
    Ok(blocks.join("\n\n"))
}

/// A code fence longer than every run of backticks in `texts`, so none of them closes it.
fn fence(texts: &[&String]) -> String {
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
            Asked::File {
                visit,
                path,
                answer,
            } => answer.send(self.opened(&visit, &path)).is_err(),
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

    /// The file `path` names in the session's directory, once the visitor may see it:
    /// a guest only what lies in `CHAT_PROJECTS`, where every link resolves.
    fn opened(&self, visit: &Visit, path: &str) -> Result<PathBuf, String> {
        let dir = self
            .known()
            .find(|(id, _, _)| *id == visit.session)
            .map(|(_, dir, _)| dir.to_owned())
            .ok_or_else(|| unseen(&visit.session))?;
        self.admits(visit.guest.as_deref(), &dir)?;
        let file = dir
            .join(path)
            .canonicalize()
            .map_err(|error| format!("{path}: {error}"))?;
        self.admits(visit.guest.as_deref(), &file)?;
        Ok(file)
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

    const TOKEN: &str = "111111:secret";

    /// `fields` signed as Telegram signs them for the bot of `TOKEN`.
    fn signed(fields: &[(&str, &str)]) -> String {
        let mut sorted: Vec<String> = fields
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        sorted.sort();
        let hash = hex(hmac::sign(&key(TOKEN), sorted.join("\n").as_bytes()).as_ref());
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .append_pair("hash", &hash)
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
        let signed = verify(&key(TOKEN), &data(&fields(&NOW.to_string())), NOW).expect("valid");
        assert_eq!(signed.user.id, 7);
        assert_eq!(signed.start, "s_1_2_YS5ycw");
    }

    #[test]
    fn init_data_altered_or_signed_for_another_bot_is_refused() {
        let valid = data(&fields(&NOW.to_string()));
        let altered = valid.replace("s_1_2_YS5ycw", "s_1_2_Yi5ycw");
        assert_eq!(
            verify(&key(TOKEN), &altered, NOW).unwrap_err(),
            "the page carries a forged signature"
        );
        assert!(verify(&key("222222:other"), &valid, NOW).is_err());
        let unsigned = valid.split("&hash=").next().expect("fields").to_owned();
        assert!(verify(&key(TOKEN), &unsigned, NOW).is_err());
    }

    #[test]
    fn init_data_older_than_a_day_is_refused() {
        let day = NOW - SIGNED_MAX;
        assert!(verify(&key(TOKEN), &data(&fields(&day.to_string())), NOW).is_ok());
        assert_eq!(
            verify(&key(TOKEN), &data(&fields(&(day - 1).to_string())), NOW).unwrap_err(),
            "the page was opened over a day ago; open the link again"
        );
    }

    fn note(from: usize, to: usize, original: &str, text: &str, suggestion: Option<&str>) -> Note {
        Note {
            path: "src/telegram.rs".to_owned(),
            from,
            to,
            original: original.to_owned(),
            text: text.to_owned(),
            suggestion: suggestion.map(str::to_owned),
        }
    }

    #[test]
    fn a_review_reads_as_a_block_per_note() {
        let notes = [
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
                path: "README.md".to_owned(),
                ..note(3, 3, "```sh", "", Some(""))
            },
        ];
        assert_eq!(
            prompt(&notes).expect("a prompt"),
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

    #[test]
    fn a_review_without_a_comment_is_refused() {
        assert!(prompt(&[]).is_err());
        assert!(prompt(&[note(3, 3, "x", " ", None)]).is_err());
        assert!(prompt(&[note(3, 2, "x", "why", None)]).is_err());
        assert!(prompt(&[note(0, 0, "x", "why", None)]).is_err());
    }
}

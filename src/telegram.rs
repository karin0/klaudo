use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use ureq::unversioned::multipart::{Form, Part};

const TIMEOUT: Duration = Duration::from_secs(30);
/// An upload is as large as the 50 MB Telegram accepts from a bot, over whatever uplink
/// the machine has.
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// How long Telegram holds a poll open with nothing to report.
const POLL_SECONDS: u64 = 50;
/// Telegram rejects a rich message past 32768 characters of rendered text, and a
/// truncated notification beats a rejected one. The markup a body carries is counted
/// here along with the text it renders, which leaves the count on the safe side.
const MAX_CHARS: usize = 32768;
/// A rejection Telegram would answer the same way stands, and the rest are worth asking
/// about again this many times. A retry can post a message twice when the answer to the
/// first was lost, which is the smaller harm, because the message a turn's thread hangs
/// from cannot be recovered once it is gone.
pub const ATTEMPTS: u32 = 3;
const BACKOFF: Duration = Duration::from_secs(1);
/// What klaude leaves on a message whose text reached a session's input box.
const SEEN: &str = "👀";

/// Whether a message reaches the phone with a sound.
#[derive(Clone, Copy)]
pub enum Sound {
    Ring,
    Silent,
}

pub struct Telegram {
    base: String,
    token: String,
    chat_id: i64,
    user_id: i64,
    /// The directories whose sessions post in `CHAT_ID`; every other goes to the user's
    /// private chat.
    projects: Vec<PathBuf>,
    agent: ureq::Agent,
}

impl Telegram {
    /// The credentials come from the file below, which every klaude process reads for
    /// itself.
    pub fn new() -> Self {
        let token = required("BOT_TOKEN");
        let chat_id = id(&required("CHAT_ID"), "CHAT_ID");
        let user_id = user(chat_id, setting("USER_ID").map(|user| id(&user, "USER_ID")));
        let projects = setting("CHAT_PROJECTS").map_or_else(Vec::new, |value| projects(&value));
        // The test stands a recording server in front of the daemon here.
        let base = setting("API_BASE").unwrap_or_else(|| "https://api.telegram.org".to_owned());
        let agent = ureq::Agent::config_builder()
            // Telegram explains a rejection in the body of the failing response.
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            base,
            token,
            chat_id,
            user_id,
            projects,
            agent,
        }
    }

    /// The id of the message it left in the chat, which is what a later message replies
    /// to.
    pub fn send(
        &self,
        chat: i64,
        markdown: &str,
        sound: Sound,
        reply_to: Option<i64>,
    ) -> Option<i64> {
        let mut body = json!({
            "chat_id": chat,
            "disable_notification": matches!(sound, Sound::Silent),
            "rich_message": {"markdown": clamp(markdown)},
        });
        if let Some(message_id) = reply_to {
            body["reply_parameters"] = replying(message_id);
        }
        self.call("sendRichMessage", &body)?["result"]["message_id"].as_i64()
    }

    /// Posts a file without a sound, under an HTML caption where one is given, since a
    /// document takes no rich message. What went wrong is handed back for whoever asked
    /// for the upload.
    pub fn document(
        &self,
        chat: i64,
        path: &Path,
        caption: Option<&str>,
        reply_to: Option<i64>,
    ) -> Result<(), String> {
        let chat = chat.to_string();
        let reply = reply_to.map(|message_id| replying(message_id).to_string());
        self.attempt("sendDocument", UPLOAD_TIMEOUT, |request| {
            let mut form = Form::new()
                .text("chat_id", &chat)
                .text("disable_notification", "true")
                .part("document", Part::file(path)?);
            if let Some(caption) = caption {
                form = form.text("caption", caption).text("parse_mode", "HTML");
            }
            if let Some(reply) = &reply {
                form = form.text("reply_parameters", reply);
            }
            request.send(form)
        })
        .map(drop)
    }

    /// Rewrites a message klaude posted, for a segment that received more after it went
    /// out.
    pub fn edit(&self, chat: i64, message_id: i64, markdown: &str) {
        self.call(
            "editMessageText",
            &json!({
                "chat_id": chat,
                "message_id": message_id,
                "rich_message": {"markdown": clamp(markdown)},
            }),
        );
    }

    /// Takes back a message klaude posted, which is how the one showing a turn's last
    /// segment goes once the answer repeating it is in the chat.
    pub fn delete(&self, chat: i64, message_id: i64) {
        self.call(
            "deleteMessage",
            &json!({
                "chat_id": chat,
                "message_id": message_id,
            }),
        );
    }

    /// Marks a message klaude typed into a terminal, which is what tells its sender the
    /// prompt was accepted while the turn is still working.
    pub fn acknowledge(&self, chat: i64, message_id: i64) {
        self.call(
            "setMessageReaction",
            &json!({
                "chat_id": chat,
                "message_id": message_id,
                "reaction": [{"type": "emoji", "emoji": SEEN}],
            }),
        );
    }

    /// Where a turn of the project at `dir` goes when nobody asked for it from the chat.
    pub fn chat(&self, dir: &Path) -> i64 {
        if listed(&self.projects, dir) {
            self.chat_id
        } else {
            self.user_id
        }
    }

    /// Whether a message is one to act on: the user's own, sent in `CHAT_ID` or in the
    /// user's private chat with the bot, whose id is the user's.
    pub fn accepts(&self, message: &Value) -> bool {
        message["from"]["id"].as_i64() == Some(self.user_id)
            && message["chat"]["id"]
                .as_i64()
                .is_some_and(|chat| chat == self.chat_id || chat == self.user_id)
    }

    /// One long poll for what the chat has sent since `offset`. Telegram holds the
    /// request open until something arrives, so the timeout has to outlast that.
    pub fn updates(&self, offset: i64) -> Option<Vec<Value>> {
        let answer = self.call(
            "getUpdates",
            &json!({
                "offset": offset,
                "timeout": POLL_SECONDS,
                "allowed_updates": ["message"],
            }),
        )?;
        Some(answer["result"].as_array()?.clone())
    }

    fn call(&self, method: &str, body: &Value) -> Option<Value> {
        // Telegram holds a poll open for the wait the request itself names, so one
        // attempt is bounded by that wait plus the patience every call gets.
        let held = Duration::from_secs(body["timeout"].as_u64().unwrap_or_default());
        self.attempt(method, TIMEOUT + held, |request| request.send_json(body))
            .ok()
    }

    /// Makes a request up to `ATTEMPTS` times, each bounded by `timeout`, and hands back
    /// Telegram's answer or the last failure.
    fn attempt(
        &self,
        method: &str,
        timeout: Duration,
        send: impl Fn(
            ureq::RequestBuilder<ureq::typestate::WithBody>,
        ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Value, String> {
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        let mut failure = String::new();
        for attempt in 1..=ATTEMPTS {
            let request = self
                .agent
                .post(&url)
                .config()
                .timeout_global(Some(timeout))
                .build();
            let outcome =
                send(request).and_then(|mut response| response.body_mut().read_json::<Value>());
            let wait = match outcome {
                Ok(answer) if answer["ok"] == Value::Bool(true) => return Ok(answer),
                Ok(answer) => {
                    failure = self.report(method, &answer.to_string());
                    // A rejection of the request itself ends the call.
                    match retry_after(&answer, attempt) {
                        Some(wait) => wait,
                        None => return Err(failure),
                    }
                }
                Err(error) => {
                    failure = self.report(method, &error.to_string());
                    backoff(attempt)
                }
            };
            if attempt < ATTEMPTS {
                std::thread::sleep(wait);
            }
        }
        Err(failure)
    }

    /// The bot token rides in every request URL, and ureq quotes the URL back in its
    /// errors, so it is masked before anything reaches the log or leaves the process.
    fn report(&self, method: &str, detail: &str) -> String {
        let masked = format!("{method}: {}", detail.replace(&self.token, "***"));
        eprintln!("{masked}");
        masked
    }
}

/// A prompt the user deleted must not take the answer to it down as well.
fn replying(message_id: i64) -> Value {
    json!({
        "message_id": message_id,
        "allow_sending_without_reply": true,
    })
}

/// How long before asking again, for a rejection that asking again can answer
/// differently: a burst Telegram wants slowed down, which names the wait it wants, or a
/// failure on its own side.
fn retry_after(answer: &Value, attempt: u32) -> Option<Duration> {
    match answer["error_code"].as_u64()? {
        429 => Some(
            answer["parameters"]["retry_after"]
                .as_u64()
                .map_or_else(|| backoff(attempt), Duration::from_secs),
        ),
        500..600 => Some(backoff(attempt)),
        _ => None,
    }
}

/// Doubling, so three attempts span a few seconds rather than a burst of their own.
fn backoff(attempt: u32) -> Duration {
    BACKOFF * 2u32.pow(attempt - 1)
}

fn clamp(markdown: &str) -> String {
    if markdown.chars().count() <= MAX_CHARS {
        return markdown.to_owned();
    }
    markdown
        .chars()
        .take(MAX_CHARS)
        .chain("…".chars())
        .collect()
}

/// Where a machine's credentials live. It is the whole of where they come from, so a
/// token changed there is the token every session uses from its next event on, and a
/// hook command is the binary's own path.
fn env_file() -> PathBuf {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".config"),
    };
    config.join("klaude").join("env")
}

/// What a credentials file assigns. A file that cannot be read stops the process, and
/// so does a line that assigns nothing, which stays out of the message because it holds
/// a credential.
fn read(path: &Path) -> HashMap<String, String> {
    let entries = match dotenvy::from_path_iter(path) {
        Ok(entries) => entries,
        Err(error) => panic!("{}: {error}", path.display()),
    };
    entries
        .map(|entry| {
            entry.unwrap_or_else(|_| panic!("{} holds a line that assigns nothing", path.display()))
        })
        .collect()
}

fn stored() -> &'static HashMap<String, String> {
    static STORED: OnceLock<HashMap<String, String>> = OnceLock::new();
    STORED.get_or_init(|| read(&env_file()))
}

fn setting(name: &str) -> Option<String> {
    stored().get(name).cloned()
}

fn required(name: &str) -> String {
    setting(name).unwrap_or_else(|| panic!("{name} is not in {}", env_file().display()))
}

fn id(value: &str, name: &str) -> i64 {
    value
        .parse()
        .unwrap_or_else(|_| panic!("{name} in {} is not an integer id", env_file().display()))
}

/// `CHAT_PROJECTS` as `PATH` spells a list, each an absolute directory.
fn projects(value: &str) -> Vec<PathBuf> {
    std::env::split_paths(value)
        .inspect(|project| {
            assert!(
                project.is_absolute(),
                "CHAT_PROJECTS in {} holds {}, which is not an absolute directory",
                env_file().display(),
                project.display()
            );
        })
        .collect()
}

/// Whether `dir` is one of `projects` or inside one, compared by whole components.
fn listed(projects: &[PathBuf], dir: &Path) -> bool {
    projects.iter().any(|project| dir.starts_with(project))
}

/// The user a chat without `USER_ID` belongs to, which is the private chat's own id. A
/// group's id is negative and belongs to nobody, so a group needs `USER_ID`.
fn user(chat: i64, user: Option<i64>) -> i64 {
    match user {
        Some(user) => user,
        None if chat > 0 => chat,
        None => panic!(
            "CHAT_ID in {} is a group, so USER_ID is needed",
            env_file().display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_body_is_truncated_on_a_character_boundary() {
        let long = "字".repeat(MAX_CHARS + 100);
        let clamped = clamp(&long);
        assert_eq!(clamped.chars().count(), MAX_CHARS + 1);
        assert!(clamped.ends_with('…'));
    }

    #[test]
    fn a_body_within_the_limit_is_untouched() {
        assert_eq!(clamp("short"), "short");
    }

    #[test]
    fn the_file_a_shell_used_to_source_reads_as_it_stands() {
        let path = std::env::temp_dir().join(format!("klaude-env-{}", std::process::id()));
        std::fs::write(
            &path,
            "# credentials\nexport BOT_TOKEN=123:abc\nCHAT_ID=-42\nUSER_ID=7\nAPI_BASE='http://localhost:1'\n",
        )
        .expect("the test writes its own file");
        let stored = read(&path);
        std::fs::remove_file(&path).expect("the file the test wrote");

        assert_eq!(stored["BOT_TOKEN"], "123:abc");
        assert_eq!(stored["CHAT_ID"], "-42");
        assert_eq!(stored["USER_ID"], "7");
        assert_eq!(stored["API_BASE"], "http://localhost:1");
        assert_eq!(stored.len(), 4);
    }

    #[test]
    fn a_private_chat_belongs_to_its_own_id_unless_a_user_is_named() {
        assert_eq!(user(42, None), 42);
        assert_eq!(user(-1001, Some(7)), 7);
        assert_eq!(user(42, Some(7)), 7);
    }

    #[test]
    fn a_project_is_listed_with_everything_under_it() {
        let projects = projects("/home/u/work:/srv/bot/");
        assert!(listed(&projects, Path::new("/home/u/work")));
        assert!(listed(&projects, Path::new("/home/u/work/api")));
        assert!(listed(&projects, Path::new("/srv/bot")));
        assert!(!listed(&projects, Path::new("/home/u/workshop")));
        assert!(!listed(&projects, Path::new("/home/u")));
        assert!(!listed(&[], Path::new("/home/u/work")));
    }

    #[test]
    #[should_panic(expected = "not an absolute directory")]
    fn a_relative_project_stops_the_process() {
        projects("/home/u/work:work");
    }

    #[test]
    #[should_panic(expected = "USER_ID is needed")]
    fn a_group_without_a_user_stops_the_process() {
        user(-1001, None);
    }

    #[test]
    fn a_rejection_is_asked_about_again_only_when_the_answer_can_differ() {
        let rejection = |answer: Value| retry_after(&answer, 1);
        assert_eq!(
            rejection(json!({"error_code": 429, "parameters": {"retry_after": 7}})),
            Some(Duration::from_secs(7))
        );
        assert_eq!(rejection(json!({"error_code": 429})), Some(BACKOFF));
        assert_eq!(rejection(json!({"error_code": 502})), Some(BACKOFF));
        assert_eq!(rejection(json!({"error_code": 400})), None);
        assert_eq!(rejection(json!({})), None);
        assert_eq!(backoff(3), BACKOFF * 4);
    }
}

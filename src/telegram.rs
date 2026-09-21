use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);
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
const ATTEMPTS: u32 = 3;
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
    agent: ureq::Agent,
}

impl Telegram {
    /// The credentials come from the file below, which every klaude process reads for
    /// itself.
    pub fn new() -> Self {
        let token = required("BOT_TOKEN");
        // Drafts are a private-chat feature, whose chat id is an integer.
        let chat_id = required("CHAT_ID")
            .parse()
            .expect("CHAT_ID is the integer id of a private chat");
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
            agent,
        }
    }

    /// The id of the message it left in the chat, which is what a later message replies
    /// to.
    pub fn send(&self, markdown: &str, sound: Sound, reply_to: Option<i64>) -> Option<i64> {
        let mut body = json!({
            "chat_id": self.chat_id,
            "disable_notification": matches!(sound, Sound::Silent),
            "rich_message": {"markdown": clamp(markdown)},
        });
        if let Some(message_id) = reply_to {
            // A prompt the user deleted must not take the answer to it down as well.
            body["reply_parameters"] = json!({
                "message_id": message_id,
                "allow_sending_without_reply": true,
            });
        }
        self.call("sendRichMessage", &body)?["result"]["message_id"].as_i64()
    }

    /// Rewrites a message klaude posted, for a segment that received more after it went
    /// out.
    pub fn edit(&self, message_id: i64, markdown: &str) {
        self.call(
            "editMessageText",
            &json!({
                "chat_id": self.chat_id,
                "message_id": message_id,
                "rich_message": {"markdown": clamp(markdown)},
            }),
        );
    }

    /// Takes back a message klaude posted, which is how the one showing a turn's last
    /// segment goes once the answer repeating it is in the chat.
    pub fn delete(&self, message_id: i64) {
        self.call(
            "deleteMessage",
            &json!({
                "chat_id": self.chat_id,
                "message_id": message_id,
            }),
        );
    }

    /// Marks a message klaude typed into a terminal, which is what tells its sender the
    /// prompt was accepted while the turn is still working.
    pub fn acknowledge(&self, message_id: i64) {
        self.call(
            "setMessageReaction",
            &json!({
                "chat_id": self.chat_id,
                "message_id": message_id,
                "reaction": [{"type": "emoji", "emoji": SEEN}],
            }),
        );
    }

    /// Whose chat this is, which is the only sender a message is accepted from.
    pub fn chat(&self) -> i64 {
        self.chat_id
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
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        // Telegram holds a poll open for the wait the request itself names, so one
        // attempt is bounded by that wait plus the patience every call gets.
        let held = Duration::from_secs(body["timeout"].as_u64().unwrap_or_default());
        for attempt in 1..=ATTEMPTS {
            let outcome = self
                .agent
                .post(&url)
                .config()
                .timeout_global(Some(TIMEOUT + held))
                .build()
                .send_json(body)
                .and_then(|mut response| response.body_mut().read_json::<Value>());
            let wait = match outcome {
                Ok(answer) if answer["ok"] == Value::Bool(true) => return Some(answer),
                Ok(answer) => {
                    self.report(method, &answer.to_string());
                    // A rejection of the request itself ends the call.
                    retry_after(&answer, attempt)?
                }
                Err(error) => {
                    self.report(method, &error.to_string());
                    backoff(attempt)
                }
            };
            if attempt < ATTEMPTS {
                std::thread::sleep(wait);
            }
        }
        None
    }

    /// The bot token rides in every request URL, and ureq quotes the URL back in its
    /// errors, so it is masked before anything reaches the log.
    fn report(&self, method: &str, detail: &str) {
        eprintln!("{method}: {}", detail.replace(&self.token, "***"));
    }
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
            "# credentials\nexport BOT_TOKEN=123:abc\nCHAT_ID=42\nAPI_BASE='http://localhost:1'\n",
        )
        .expect("the test writes its own file");
        let stored = read(&path);
        std::fs::remove_file(&path).expect("the file the test wrote");

        assert_eq!(stored["BOT_TOKEN"], "123:abc");
        assert_eq!(stored["CHAT_ID"], "42");
        assert_eq!(stored["API_BASE"], "http://localhost:1");
        assert_eq!(stored.len(), 3);
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

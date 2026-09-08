use std::time::Duration;

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);
/// How long Telegram holds a poll open with nothing to report.
const POLL_SECONDS: u64 = 50;
/// Telegram rejects a message body past 4096 characters, and a truncated notification
/// beats a rejected one.
const MAX_CHARS: usize = 4000;

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
    /// A hook inherits the environment the command that started it was given, which is
    /// where the credentials come from.
    pub fn from_env() -> Self {
        let token = required("BOT_TOKEN");
        // Drafts are a private-chat feature, whose chat id is an integer.
        let chat_id = required("CHAT_ID")
            .parse()
            .expect("CHAT_ID is the integer id of a private chat");
        // The test stands a recording server in front of the daemon here.
        let base =
            std::env::var("API_BASE").unwrap_or_else(|_| "https://api.telegram.org".to_owned());
        let agent = ureq::Agent::config_builder()
            // Covers the long poll, whose own deadline is the one Telegram honours.
            .timeout_global(Some(TIMEOUT + Duration::from_secs(POLL_SECONDS)))
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

    /// Frames sharing a draft id animate into each other; a new id replaces the draft.
    pub fn draft(&self, draft_id: i64, markdown: &str) {
        self.call(
            "sendRichMessageDraft",
            &json!({
                "chat_id": self.chat_id,
                "draft_id": draft_id,
                "rich_message": {"markdown": clamp(markdown)},
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
        let outcome = self
            .agent
            .post(&url)
            .send_json(body)
            .and_then(|mut response| response.body_mut().read_json::<Value>());
        match outcome {
            Ok(answer) if answer["ok"] == Value::Bool(true) => Some(answer),
            Ok(answer) => {
                self.report(method, &answer.to_string());
                None
            }
            Err(error) => {
                self.report(method, &error.to_string());
                None
            }
        }
    }

    /// The bot token rides in every request URL, and ureq quotes the URL back in its
    /// errors, so it is masked before anything reaches the log.
    fn report(&self, method: &str, detail: &str) {
        eprintln!("{method}: {}", detail.replace(&self.token, "***"));
    }
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

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not in the environment"))
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
}

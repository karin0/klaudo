use serde::Deserialize;
use serde_json::{Map, Value};

/// Fields that identify the invocation rather than describe it, dropped from the
/// verbatim report an unrecognised event falls back to.
const BOILERPLATE: [&str; 5] = [
    "transcript_path",
    "prompt_id",
    "permission_mode",
    "effort",
    "agent_type",
];

/// Marks the end of a turn, so a chat holding several projects can be filtered down to
/// the replies that finished a piece of work.
const TAG: &str = "#claude";

/// One hook invocation as Claude Code writes it on stdin. Fields no arm below reads
/// survive in `rest`, so an event this program has never seen still reports what it
/// carries instead of arriving empty.
#[derive(Deserialize)]
pub struct Event {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub delta: Option<String>,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

impl Event {
    fn tag(&self) -> String {
        match self.hook_event_name.as_str() {
            // The turn opens with what was asked; the tag belongs to what closes it.
            "UserPromptSubmit" => String::new(),
            "Stop" => TAG.to_owned(),
            "StopFailure" => format!("{TAG} #failed"),
            "Notification" => format!("{TAG} #input"),
            other => format!("{TAG} #{other}"),
        }
    }

    fn body(&self) -> String {
        match self.hook_event_name.as_str() {
            "UserPromptSubmit" => self.prompt.clone().unwrap_or_default(),
            "Stop" => self.last_assistant_message.clone().unwrap_or_default(),
            "StopFailure" => self.error.clone().unwrap_or_else(|| self.residue()),
            "Notification" => self.message.clone().unwrap_or_else(|| self.residue()),
            _ => self.residue(),
        }
    }

    fn residue(&self) -> String {
        let kept: Map<String, Value> = self
            .rest
            .iter()
            .filter(|(key, _)| !BOILERPLATE.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        serde_json::to_string(&kept).expect("a parsed object re-serializes")
    }
}

/// Where the work is happening.
pub fn project(cwd: &str) -> String {
    format!("**{}**", cwd.rsplit('/').next().unwrap_or(cwd))
}

/// The line every message opens with: where the work is, which session, and which turn
/// of that session. The prompt id is what tells one turn from the next.
pub fn head(project: &str, session: &str, prompt: Option<&str>) -> String {
    let short = |id: &str| id.chars().take(8).collect::<String>();
    let address = match prompt {
        Some(prompt) => format!("{}/{}", short(session), short(prompt)),
        None => short(session),
    };
    format!("{project} `{address}`")
}

/// An untagged title is what a message in the middle of a turn carries.
pub fn compose(head: &str, took: &str, tag: &str, body: &str) -> String {
    let title = format!("{head}{took}  {tag}");
    format!("{}\n\n{body}", title.trim_end())
}

pub fn message(event: &Event, head: &str, took: &str) -> String {
    compose(head, took, &event.tag(), &event.body())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(json: serde_json::Value) -> Event {
        serde_json::from_value(json).expect("fixture")
    }

    #[test]
    fn a_project_is_the_last_segment_of_its_directory() {
        assert_eq!(project("/home/user/scratch"), "**scratch**");
    }

    #[test]
    fn a_head_addresses_the_turn_and_falls_back_to_the_session() {
        assert_eq!(
            head("**p**", "0123456789abcdef", Some("fedcba9876543210")),
            "**p** `01234567/fedcba98`"
        );
        assert_eq!(head("**p**", "0123456789abcdef", None), "**p** `01234567`");
    }

    #[test]
    fn a_title_without_a_tag_ends_at_the_elapsed_time() {
        assert_eq!(compose("**p**", " 12s", "", "text"), "**p** 12s\n\ntext");
    }

    #[test]
    fn stop_reports_the_last_assistant_message() {
        let stop = event(serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "s",
            "last_assistant_message": "done",
        }));
        assert_eq!(
            message(&stop, "**p**", " 12s"),
            "**p** 12s  #claude\n\ndone"
        );
    }

    #[test]
    fn a_prompt_reports_what_was_asked_and_opens_the_thread() {
        let submit = event(serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "s",
            "prompt": "what does it do",
        }));
        assert_eq!(message(&submit, "**p**", ""), "**p**\n\nwhat does it do");
    }

    #[test]
    fn an_unread_event_reports_what_it_carries_without_the_boilerplate() {
        let odd = event(serde_json::json!({
            "hook_event_name": "PreCompact",
            "session_id": "s",
            "transcript_path": "/tmp/t.jsonl",
            "trigger": "auto",
        }));
        assert_eq!(
            message(&odd, "**p**", ""),
            "**p**  #claude #PreCompact\n\n{\"trigger\":\"auto\"}"
        );
    }

    #[test]
    fn a_failure_without_an_error_field_still_reports() {
        let failed = event(serde_json::json!({
            "hook_event_name": "StopFailure",
            "session_id": "s",
            "reason": "overloaded",
        }));
        assert_eq!(
            message(&failed, "**p**", ""),
            "**p**  #claude #failed\n\n{\"reason\":\"overloaded\"}"
        );
    }
}

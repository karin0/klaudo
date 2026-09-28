//! Messages and presses from the chat, and the terminals they are typed into.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::hook;
use crate::telegram::Place;
use crate::tmux;

use super::render::code;
use super::turn::Ask;
use super::{Machine, Opening, Trail, now_millis};

/// What a message from the chat addresses when it opens a conversation rather than
/// continuing one.
pub(super) const NEW: &str = "new";
const RESUME: &str = "resume";
const USAGE: &str = "usage";
/// What a button picking one session of a `/resume` menu carries ahead of its id.
const SESSION: &str = "session";
/// What the button leading a `/resume` menu of sessions back to its projects carries.
const PROJECTS: &str = "projects";
/// What a `/resume` menu of projects reads.
const RESUMING: &str = "Resume a conversation in:";
/// What the chat's command menu offers, each with the line it is listed under.
pub(super) const COMMANDS: &[(&str, &str)] = &[
    (NEW, "Open a conversation in a directory"),
    (RESUME, "Resume a recent conversation"),
    (
        USAGE,
        "Show the plan's limits and the context of the conversation it replies to",
    ),
    ("compact", "Compact the conversation it replies to"),
];

/// How many choices a menu offers, which a phone shows without scrolling.
const MENU_MAX: usize = 8;

/// A button the chat pressed on a menu the daemon posted. The menu is the message the
/// button hangs from, and `data` names the button.
#[derive(Deserialize)]
pub(super) struct Press {
    pub(super) id: String,
    from: Value,
    pub(super) message: Value,
    data: String,
}

impl Machine {
    /// A session is ready for input once it says so, which is after the dialog that
    /// asks whether its folder is trusted. A conversation opened from the chat is
    /// waiting for exactly this to type its first prompt. A resumed session takes every
    /// ask that waited for it, while each ask that opened a new conversation had a
    /// window of its own.
    pub(super) fn started(&mut self, id: &str) {
        let Some(dir) = self.sessions.get(id).map(|session| session.dir.clone()) else {
            return;
        };
        let mut asks: Vec<Ask> = self
            .opening
            .extract_if(.., |opening| opening.resume.as_deref() == Some(id))
            .map(|opening| opening.ask)
            .collect();
        if asks.is_empty()
            && let Some(index) = self
                .opening
                .iter()
                .position(|opening| opening.resume.is_none() && opening.dir == dir)
        {
            asks.push(self.opening.remove(index).ask);
        }
        for ask in asks {
            self.send(id, ask);
        }
    }

    /// A message from the chat. What it replies to says where it goes: a message from a
    /// session reaches that session, and the message `/new` left behind opens a
    /// conversation in the directory it names. A message replying to nothing goes to
    /// the session heard from last in its place.
    pub(super) fn chat(&mut self, message: &Value) {
        let Some(place) = self.admitted(&message["from"], message) else {
            return;
        };
        let text = message["text"].as_str().unwrap_or_default().trim();
        if text.is_empty() {
            return;
        }
        let Some(carrier) = message["message_id"].as_i64() else {
            return;
        };
        let replied = &message["reply_to_message"];
        match command(text) {
            Some((NEW, "")) => {
                let lead = self.reached(place, replied);
                if let Some(buttons) = self.choices(place, NEW, lead) {
                    self.telegram
                        .menu(place, "Open a conversation in:", &buttons);
                }
                return;
            }
            Some((NEW, argument)) => {
                self.anchor(place, argument);
                return;
            }
            Some((RESUME, _)) => {
                self.resume(place, self.reached(place, replied));
                return;
            }
            Some((USAGE, _)) => {
                self.usage(place, carrier, replied);
                return;
            }
            _ => {}
        }
        let ask = Ask {
            text: unaddressed(text),
            place,
            message: carrier,
        };
        // A session that exited without saying so is resumed rather than typed into.
        self.sweep();
        match self.addressee(place, replied) {
            Ok(Some(address)) if address == NEW => match body(replied) {
                Some(cwd) => self.open(PathBuf::from(cwd), None, ask),
                None => self.say(place, "that anchor names no directory"),
            },
            Ok(Some(address)) => self.send(&address, ask),
            Ok(None) => self.say(
                place,
                "no session is running here; `/new <directory>` opens one",
            ),
            Err(error) => self.say(place, error),
        }
    }

    /// Where a message goes: the session or anchor the message it replies to names, or
    /// for a message replying to nothing, the session heard from last in its place. A
    /// message outside every topic of a private chat in topic mode opens a topic whose
    /// name is implicit, so a topic like that holding no session reaches the sessions
    /// outside every topic, where the turns started in the terminal are posted.
    pub(super) fn addressee(
        &self,
        place: Place,
        replied: &Value,
    ) -> Result<Option<String>, &'static str> {
        // A message in a topic that replies to nothing replies to the service message
        // that opened the topic.
        let opened = &replied["forum_topic_created"];
        if replied.is_null() || opened.is_object() {
            let outside = (opened["is_name_implicit"] == Value::Bool(true)).then_some(Place {
                topic: None,
                ..place
            });
            return Ok(self
                .latest(place)
                .or_else(|| outside.and_then(|outside| self.latest(outside))));
        }
        address(replied)
            .map(Some)
            .ok_or("the message replied to names no session")
    }

    /// The directory of the session a message would reach, which leads a menu it asks for.
    fn reached(&self, place: Place, replied: &Value) -> Option<&Path> {
        let id = self.addressee(place, replied).ok()??;
        self.known()
            .find(|(known, _, _, _)| known.starts_with(&id))
            .map(|(_, dir, _, _)| dir)
    }

    /// Where a message came from, when it is one to act on.
    fn admitted(&self, sender: &Value, message: &Value) -> Option<Place> {
        let place = self.telegram.accepts(sender, message);
        if place.is_none() {
            // The ids to put in the env file are read from here.
            let chat = &message["chat"];
            eprintln!(
                "ignored: chat {} {:?} from {}",
                chat["id"],
                chat["title"].as_str().unwrap_or_default(),
                sender["id"]
            );
        }
        place
    }

    /// A button pressed on a menu. Its label is what it picks, so a menu posted before a
    /// restart still works.
    pub(super) fn press(&mut self, press: &Press) {
        self.telegram.answer(&press.id);
        let Some(place) = self.admitted(&press.from, &press.message) else {
            return;
        };
        let Some(menu) = press.message["message_id"].as_i64() else {
            return;
        };
        let Some(label) = label(&press.message, &press.data) else {
            return;
        };
        if press.data == PROJECTS {
            if let Some(buttons) = self.choices(place, RESUME, None) {
                self.telegram.remenu(place.chat, menu, RESUMING, &buttons);
            }
            return;
        }
        match press.data.split_once(' ') {
            // An anchor opens the reply box only as it arrives, so it is a message of
            // its own, and a menu left behind is one mistaken press from a second one.
            Some((NEW, _)) => {
                if self.anchor(place, &label).is_some() {
                    self.telegram.delete(place.chat, menu);
                }
            }
            Some((RESUME, _)) => {
                if let Some((text, buttons)) = self.conversations(place, &label) {
                    self.telegram.remenu(place.chat, menu, &text, &buttons);
                }
            }
            Some((SESSION, id)) => {
                if self.resumption(place, id).is_some() {
                    self.telegram.delete(place.chat, menu);
                }
            }
            _ => eprintln!("press: {}", press.data),
        }
    }

    /// The directories the sessions of `chat` ran in, `lead` first and then the one heard
    /// from last.
    fn projects(&self, chat: i64, lead: Option<&Path>) -> Vec<PathBuf> {
        let mut seen: Vec<(u64, &Path)> =
            self.known().map(|(_, dir, seen, _)| (seen, dir)).collect();
        seen.sort_by_key(|(seen, _)| std::cmp::Reverse(*seen));
        let mut projects: Vec<PathBuf> = Vec::new();
        for dir in lead.into_iter().chain(seen.into_iter().map(|(_, dir)| dir)) {
            if self.telegram.chat(dir) == chat
                && dir.is_dir()
                && !projects.iter().any(|project| project == dir)
            {
                projects.push(dir.to_owned());
            }
        }
        projects.truncate(MENU_MAX);
        projects
    }

    /// Every session the daemon knows of, running or exited, with where it ran, when
    /// it was last heard from in Unix milliseconds, and its trail.
    fn known(&self) -> impl Iterator<Item = (&str, &Path, u64, &Trail)> {
        self.sessions
            .iter()
            .map(|(id, session)| {
                (
                    id.as_str(),
                    session.dir.as_path(),
                    session.seen,
                    &session.trail,
                )
            })
            .chain(self.ended.iter().map(|ended| {
                (
                    ended.id.as_str(),
                    ended.dir.as_path(),
                    ended.seen,
                    &ended.trail,
                )
            }))
    }

    /// A `/resume` menu: the sessions of the project `lead`, or with none, the projects
    /// of the chat `place` is in.
    fn resume(&self, place: Place, lead: Option<&Path>) {
        let menu = match lead {
            Some(dir) => self.conversations(place, &tilde(dir)),
            None => self
                .choices(place, RESUME, None)
                .map(|buttons| (RESUMING.to_owned(), buttons)),
        };
        if let Some((text, buttons)) = menu {
            self.telegram.menu(place, &text, &buttons);
        }
    }

    /// The text and buttons of a menu of a project's sessions, the last button leading
    /// back to the projects.
    fn conversations(&self, place: Place, label: &str) -> Option<(String, Vec<(String, String)>)> {
        let Some(dir) = expand(label) else {
            self.say(place, &format!("{} is not a directory", code(label)));
            return None;
        };
        let now = now_millis();
        let mut sessions: Vec<_> = self.known().filter(|(_, ran, _, _)| *ran == dir).collect();
        sessions.sort_by_key(|(_, _, seen, _)| std::cmp::Reverse(*seen));
        let mut buttons: Vec<(String, String)> = sessions
            .iter()
            .take(MENU_MAX)
            .map(|(id, _, seen, trail)| {
                let age = ago(Duration::from_millis(now.saturating_sub(*seen)));
                let short = hook::address(id, None);
                let label = match trail.prompt.as_str() {
                    "" => format!("{short} · {age}"),
                    prompt => format!("{short} · {age} · {prompt}"),
                };
                (label, format!("{SESSION} {id}"))
            })
            .collect();
        if buttons.is_empty() {
            self.say(place, &format!("no session has run in {}", code(label)));
            return None;
        }
        buttons.push(("« Projects".to_owned(), PROJECTS.to_owned()));
        Some((format!("Resume a conversation in {label}:"), buttons))
    }

    /// An anchor addressed to session `id`, replying to the last message it left in this
    /// place, which a tap on the quotation scrolls back to. A reply to the anchor goes
    /// where a reply to any of its messages would.
    fn resumption(&self, place: Place, id: &str) -> Option<i64> {
        let Some((_, dir, _, trail)) = self.known().find(|(known, _, _, _)| *known == id) else {
            self.say(
                place,
                &format!("{} is not a session this daemon has seen", code(id)),
            );
            return None;
        };
        let head = hook::head(&hook::project(dir), id, None);
        let message = hook::compose(&head, "", "", &hook::prose(&tilde(dir)));
        let last = trail
            .last
            .and_then(|(posted, message)| (posted == place).then_some(message));
        let placeholder = format!("prompt for {}", hook::address(id, None));
        self.telegram.anchor(place, &message, &placeholder, last)
    }

    /// The buttons of a menu of the projects of the chat `place` is in, `lead` first,
    /// each carrying `command` and its place in the menu.
    fn choices(
        &self,
        place: Place,
        command: &str,
        lead: Option<&Path>,
    ) -> Option<Vec<(String, String)>> {
        let buttons: Vec<(String, String)> = self
            .projects(place.chat, lead)
            .iter()
            .enumerate()
            .map(|(index, dir)| (tilde(dir), format!("{command} {index}")))
            .collect();
        if buttons.is_empty() {
            self.say(
                place,
                "no project has run here yet; `/new <directory>` opens one",
            );
            return None;
        }
        Some(buttons)
    }

    /// The session in `place` heard from last, which is where a message that replies to
    /// nothing goes. A session posting elsewhere stays out of reach, so a project never
    /// answers in a chat it is not posted to, and a topic holds its own conversations.
    fn latest(&self, place: Place) -> Option<String> {
        self.sessions
            .iter()
            .filter(|(_, session)| self.thread(session).place == place)
            .max_by_key(|(_, session)| session.seen)
            .map(|(id, _)| id.clone())
    }

    /// Posts a message to reply to with the first prompt of a new conversation. Nothing
    /// is started yet, so an anchor left alone costs nothing.
    fn anchor(&self, place: Place, argument: &str) -> Option<i64> {
        let Some(cwd) = expand(argument).filter(|cwd| cwd.is_dir()) else {
            self.say(place, &format!("{} is not a directory", code(argument)));
            return None;
        };
        let head = hook::head(&hook::project(&cwd), NEW, None);
        let message = hook::compose(&head, "", "", &hook::prose(&cwd.to_string_lossy()));
        let placeholder = format!("first prompt in {}", tilde(&cwd));
        self.telegram.anchor(place, &message, &placeholder, None)
    }

    /// Opens a window for a conversation, a new one or the session `resume` names, and
    /// keeps its first prompt until the session there reports that it is ready. A
    /// session already being resumed gets no second window, which would run it twice.
    fn open(&mut self, dir: PathBuf, resume: Option<String>, ask: Ask) {
        let resuming = self
            .opening
            .iter()
            .find(|opening| resume.is_some() && opening.resume == resume)
            .map(|opening| opening.pane.clone());
        let pane = match resuming.map_or_else(|| tmux::open(&dir, resume.as_deref()), Ok) {
            Ok(pane) => pane,
            Err(error) => {
                self.say(ask.place, &format!("tmux: {}", hook::prose(&error)));
                return;
            }
        };
        self.opening.push(Opening {
            dir,
            resume,
            pane,
            ask,
        });
    }

    /// Types into the session whose id starts with `address`, resuming it first when it
    /// has exited.
    fn send(&mut self, address: &str, ask: Ask) {
        let place = ask.place;
        let Some((id, session)) = self.sessions.iter().find(|(id, _)| id.starts_with(address))
        else {
            match self
                .ended
                .iter()
                .find(|ended| ended.id.starts_with(address))
            {
                Some(ended) => self.open(ended.dir.clone(), Some(ended.id.clone()), ask),
                None => self.say(
                    place,
                    &format!("`{address}` is not a session this daemon has seen"),
                ),
            }
            return;
        };
        let id = id.clone();
        let Some(pane) = session.pane.clone() else {
            // Naming the terminal is what tells a session started as a background job,
            // which runs on a pty of its own, from one whose pane went away.
            let short = &id[..8.min(id.len())];
            self.say(
                place,
                &match tmux::controlling_tty(session.pid) {
                    Some(tty) => format!("`{short}` is on `{tty}`, which no tmux pane holds"),
                    None => format!("`{short}` has no terminal to type into"),
                },
            );
            return;
        };
        let pid = session.pid;
        if !pane.holds(pid) {
            // A terminal draws whatever it likes, so it goes in a fence rather than
            // through the markdown parser.
            let screen = pane
                .screen()
                .map(|screen| format!("\n\n```\n{screen}\n```"))
                .unwrap_or_default();
            self.say(
                place,
                &format!("that terminal no longer holds the session{screen}"),
            );
            return;
        }
        if let Err(error) = pane.deliver(&ask.text) {
            self.say(place, &format!("tmux: {}", hook::prose(&error)));
            return;
        }
        self.sessions
            .get_mut(&id)
            .expect("the session just found")
            .asked
            .push_back(ask);
    }
}

/// The address in the head of a message Klaŭdo posted, which is the session it belongs
/// to or the anchor of a conversation that has not started.
fn address(message: &Value) -> Option<String> {
    let code = headed(message)
        .or_else(|| coded(&message["caption"], &message["caption_entities"]))
        .or_else(|| coded(&message["text"], &message["entities"]))?;
    let session = code.split('/').next()?;
    (!session.is_empty()).then(|| session.to_owned())
}

fn headed(message: &Value) -> Option<String> {
    let spans = paragraph(message, 0)?.as_array()?;
    let code = spans.iter().find(|span| span["type"] == "code")?;
    Some(plain(&code["text"]))
}

/// The code span of a message Klaŭdo posted as text with entities: a file, whose head
/// is its caption, or an HTML message. Entities count UTF-16 code units.
fn coded(text: &Value, entities: &Value) -> Option<String> {
    let text: Vec<u16> = text.as_str()?.encode_utf16().collect();
    let code = entities
        .as_array()?
        .iter()
        .find(|entity| entity["type"] == "code")?;
    let start = usize::try_from(code["offset"].as_u64()?).ok()?;
    let end = start + usize::try_from(code["length"].as_u64()?).ok()?;
    String::from_utf16(text.get(start..end)?).ok()
}

fn body(message: &Value) -> Option<String> {
    let body = plain(paragraph(message, 1)?);
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_owned())
}

/// A message Klaŭdo posted comes back as the blocks Telegram rendered its markdown
/// into, so its head and its body are the first two paragraphs of that.
fn paragraph(message: &Value, index: usize) -> Option<&Value> {
    let block = message["rich_message"]["blocks"].get(index)?;
    (block["type"] == "paragraph").then(|| &block["text"])
}

/// The text of a paragraph, or of one span of it, which Telegram writes as a bare
/// string wherever it carries no formatting of its own.
fn plain(node: &Value) -> String {
    match node {
        Value::String(text) => text.clone(),
        Value::Array(spans) => spans.iter().map(plain).collect(),
        Value::Object(_) => plain(&node["text"]),
        _ => String::new(),
    }
}

/// How long ago, in the largest unit it fills.
pub(super) fn ago(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

/// A command the daemon answers and its argument. A group's command menu names the bot
/// a command is for, as `/new@bot`.
fn command(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (word, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let name = word.split_once('@').map_or(word, |(name, _)| name);
    Some((name, argument.trim()))
}

/// `text` without the `@<bot>` a group's command menu appends to a command, which Claude
/// Code would take for part of the command's name.
fn unaddressed(text: &str) -> String {
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    match text[..end].find('@') {
        Some(at) if text.starts_with('/') => format!("{}{}", &text[..at], &text[end..]),
        _ => text.to_owned(),
    }
}

/// The label of the button in a menu that carries `data`.
fn label(menu: &Value, data: &str) -> Option<String> {
    menu["reply_markup"]["inline_keyboard"]
        .as_array()?
        .iter()
        .flat_map(|row| row.as_array().into_iter().flatten())
        .find(|button| button["callback_data"] == data)?["text"]
        .as_str()
        .map(str::to_owned)
}

/// A directory as it would be typed in the chat, which `expand` reads back.
pub(super) fn tilde(dir: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|home| dir.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

/// A directory as it would be typed in the chat, where a leading `~` is the only shell
/// spelling worth honouring because nothing here runs a shell.
fn expand(argument: &str) -> Option<PathBuf> {
    if argument.is_empty() {
        return None;
    }
    let path = match argument.strip_prefix('~') {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest.trim_start_matches('/')),
        None => PathBuf::from(argument),
    };
    path.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message Klaŭdo posted, as Telegram hands it back in the reply to it.
    fn posted(paragraphs: &[Value]) -> Value {
        let blocks: Vec<Value> = paragraphs
            .iter()
            .map(|text| serde_json::json!({"type": "paragraph", "text": text}))
            .collect();
        serde_json::json!({"rich_message": {"blocks": blocks}})
    }

    fn head_of(address: &str) -> Value {
        serde_json::json!([
            {"type": "bold", "text": "project"},
            " ",
            {"type": "code", "text": address},
            " 12s ",
            {"type": "hashtag", "text": "#claude", "hashtag": "claude"},
        ])
    }

    #[test]
    fn a_reply_is_addressed_by_the_head_of_the_message_it_answers() {
        let turn = posted(&[head_of("01234567/fedcba98"), "done".into()]);
        assert_eq!(address(&turn).as_deref(), Some("01234567"));
        let anchor = posted(&[head_of(NEW), "/home/u/p".into()]);
        assert_eq!(address(&anchor).as_deref(), Some(NEW));
        assert_eq!(address(&posted(&["plain".into()])), None);
        let document = serde_json::json!({
            "caption": "🐱 01234567/fedcba98 5s",
            "caption_entities": [
                {"type": "bold", "offset": 0, "length": 2},
                {"type": "code", "offset": 3, "length": 17},
            ],
        });
        assert_eq!(address(&document).as_deref(), Some("01234567"));
        assert_eq!(
            address(&serde_json::json!({"text": "from the phone"})),
            None
        );
    }

    #[test]
    fn an_anchor_carries_its_directory_in_its_body() {
        assert_eq!(
            body(&posted(&[head_of(NEW), "/home/u/p".into()])).as_deref(),
            Some("/home/u/p")
        );
        // A path Telegram found something to format in still reads as its own text.
        let split = serde_json::json!(["/home/u/", {"type": "italic", "text": "p_q_r"}]);
        assert_eq!(
            body(&posted(&[head_of(NEW), split])).as_deref(),
            Some("/home/u/p_q_r")
        );
        assert_eq!(body(&posted(&[head_of(NEW)])), None);
    }

    #[test]
    fn an_age_reads_in_the_largest_unit_it_fills() {
        assert_eq!(ago(Duration::from_secs(59)), "59s ago");
        assert_eq!(ago(Duration::from_secs(3599)), "59m ago");
        assert_eq!(ago(Duration::from_secs(86399)), "23h ago");
        assert_eq!(ago(Duration::from_hours(72)), "3d ago");
    }

    #[test]
    fn a_command_is_read_with_or_without_the_bot_it_names() {
        assert_eq!(command("/new ~/p"), Some(("new", "~/p")));
        assert_eq!(command("/new@klaudo_bot  ~/p "), Some(("new", "~/p")));
        assert_eq!(command("/new@klaudo_bot"), Some(("new", "")));
        assert_eq!(
            command("/compact keep it short"),
            Some(("compact", "keep it short"))
        );
        assert_eq!(command("new"), None);
    }

    #[test]
    fn a_command_is_typed_without_the_bot_it_names() {
        assert_eq!(unaddressed("/compact@klaudo_bot"), "/compact");
        assert_eq!(
            unaddressed("/compact@klaudo_bot keep a@b"),
            "/compact keep a@b"
        );
        assert_eq!(unaddressed("/compact keep a@b"), "/compact keep a@b");
        assert_eq!(unaddressed("mail a@b"), "mail a@b");
    }

    #[test]
    fn a_directory_under_home_is_written_from_a_tilde_and_read_back() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("a home"));
        assert_eq!(tilde(&home), "~");
        assert_eq!(tilde(&home.join("dev/p")), "~/dev/p");
        assert_eq!(tilde(Path::new("/srv/p")), "/srv/p");
    }

    #[test]
    fn a_pressed_button_is_found_by_the_data_it_carries() {
        let menu = serde_json::json!({"reply_markup": {"inline_keyboard": [
            [{"text": "~/a", "callback_data": "new 0"}],
            [{"text": "~/b", "callback_data": "new 1"}],
        ]}});
        assert_eq!(label(&menu, "new 1").as_deref(), Some("~/b"));
        assert_eq!(label(&menu, "new 2"), None);
    }

    #[test]
    fn a_directory_is_expanded_from_a_leading_tilde() {
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(expand("~").as_deref(), Some(Path::new(&home)));
        assert_eq!(expand(""), None);
        assert_eq!(expand("/definitely/not/here"), None);
    }
}

//! `/issues`: the issues of a project, listed or filed by a program the env file names.

use kuriero::Message;

use crate::hook;
use crate::telegram::{Place, Sound};

use super::Machine;
use super::chat::NEW;

/// What a guest asking for the issues is answered with.
pub(super) const GUEST: &str = "a guest cannot list or file issues";

impl Machine {
    /// Runs `ISSUES_COMMAND` in the directory of the session a message replying to
    /// `replied` would reach, with `text` as its one argument when given, and posts what
    /// it prints under the session's head, so a reply reaches the session.
    pub(super) fn issues(&self, place: Place, asked: i64, replied: Option<&Message>, text: &str) {
        let Some(mut command) = self.issues.command(&self.telegram, place) else {
            return;
        };
        let address = match self.addressee(place, replied) {
            Ok(address) => address.filter(|address| address != NEW),
            Err(error) => return self.say(place, error),
        };
        let Some((id, dir, _)) =
            address.and_then(|address| self.known().find(|(id, _, _)| id.starts_with(&address)))
        else {
            return self.say(place, "no session has run here to show the issues of");
        };
        let head = hook::Head::new(dir, id, None);
        command.current_dir(dir);
        if !text.is_empty() {
            command.arg(text);
        }
        self.issues.spawn(
            &self.telegram,
            command,
            place,
            Some(asked),
            move |telegram, output| {
                let printed = String::from_utf8_lossy(&output);
                let body = match printed.trim() {
                    "" => "`ISSUES_COMMAND` printed nothing",
                    printed => printed,
                };
                let said = hook::compose(&head, "", "", body).markdown;
                telegram.send(place, &said, Sound::Silent, Some(asked));
            },
        );
    }
}

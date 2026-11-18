//! A session's terminal, shown on another device by a program the env file names.

use std::process::Command;

use kuriero::Message;

use crate::telegram::{Admitted, Place};
use crate::tmux::{Pane, Start};

use super::chat::{ATTACH, NEW, NOTHING_RAN, unseen};
use super::program::Program;
use super::{Awaiting, Machine};

/// What a guest asking to attach to a terminal is answered with.
pub(super) const GUEST: &str = "a guest cannot attach to a terminal";

/// The button under a message about `pane` that attaches to it, where `attach`,
/// `ATTACH_COMMAND`, is set.
pub(super) fn button(attach: &Program, pane: &Pane) -> Vec<(String, String)> {
    attach
        .is_set()
        .then(|| ("Attach".to_owned(), format!("{ATTACH} {}", pane.id)))
        .into_iter()
        .collect()
}

impl Machine {
    /// Shows the pane of the session a message replying to `replied` would reach. A
    /// session that has exited is resumed in a window of its own first, which shows it
    /// starting.
    pub(super) fn attach(&mut self, place: Place, asked: i64, replied: Option<&Message>) {
        let Some(command) = self.attach.command(&self.telegram, place) else {
            return;
        };
        // A session that exited without saying so is resumed rather than shown.
        self.sweep();
        let address = match self.addressee(place, replied) {
            Ok(Some(address)) if address == NEW => {
                return self.say(
                    place,
                    "a conversation has no terminal before its first prompt",
                );
            }
            Ok(Some(address)) => address,
            Ok(None) => return self.say(place, NOTHING_RAN),
            Err(error) => return self.say(place, error),
        };
        if let Some(pane) = self.pane(place, &address) {
            self.display(command, place, &pane, Some(asked));
        }
    }

    /// The press on the button of the word about a window whose session was late to
    /// start, which attaches to pane `id` while it is open.
    pub(super) fn attach_window(&mut self, from: &Admitted, id: &str) {
        if from.guest.is_some() {
            return self.say(from.place, GUEST);
        }
        let Some(command) = self.attach.command(&self.telegram, from.place) else {
            return;
        };
        let pane = self
            .opening
            .iter()
            .map(|opening| &opening.pane)
            .chain(
                self.sessions
                    .values()
                    .filter_map(|session| session.pane.as_ref()),
            )
            .find(|pane| pane.id == id && pane.open());
        match pane {
            Some(pane) => self.display(command, from.place, pane, None),
            None => self.say(from.place, "that window has closed"),
        }
    }

    /// Runs `command`, `ATTACH_COMMAND`, with `pane` and its tmux server's socket as its
    /// arguments, acknowledging the message `asked` when one asked.
    fn display(&self, mut command: Command, place: Place, pane: &Pane, asked: Option<i64>) {
        command.arg(&pane.id).arg(&pane.server);
        self.attach
            .spawn(&self.telegram, command, place, asked, move |telegram, _| {
                if let Some(asked) = asked {
                    telegram.acknowledge(place.chat, asked);
                }
            });
    }

    /// The pane of the session whose id starts with `address`, or of the window that
    /// resumes it when it has exited.
    fn pane(&mut self, place: Place, address: &str) -> Option<Pane> {
        if let Some((id, _)) = self.addressed(address) {
            return self
                .terminal(&id.clone())
                .inspect_err(|error| self.say(place, error))
                .ok();
        }
        let Some(ended) = self
            .ended
            .iter()
            .find(|ended| ended.id.starts_with(address))
        else {
            self.say(place, &unseen(address));
            return None;
        };
        let (dir, id) = (ended.dir.clone(), ended.id.clone());
        self.open(dir, Start::Resume(id), Awaiting::Terminal(place))
            .inspect_err(|error| self.say(place, error))
            .ok()
    }
}

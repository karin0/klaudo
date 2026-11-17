//! A session's terminal, shown on another device by a program the env file names.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kuriero::Message;

use crate::process;
use crate::telegram::{Admitted, Place, Sound};
use crate::tmux::{Pane, Start};

use super::chat::{NEW, NOTHING_RAN, unseen};
use super::{Awaiting, Machine};

/// What a guest asking to attach to a terminal is answered with.
pub(super) const GUEST: &str = "a guest cannot attach to a terminal";

impl Machine {
    /// Shows the pane of the session a message replying to `replied` would reach. A
    /// session that has exited is resumed in a window of its own first, which shows it
    /// starting.
    pub(super) fn attach(&mut self, place: Place, asked: i64, replied: Option<&Message>) {
        let Some(program) = self.program(place) else {
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
            self.display(&program, place, pane, Some(asked));
        }
    }

    /// The press on the button of the word about a window whose session was late to
    /// start, which attaches to pane `id` while it is open.
    pub(super) fn attach_window(&mut self, from: &Admitted, id: &str) {
        if from.guest.is_some() {
            return self.say(from.place, GUEST);
        }
        let Some(program) = self.program(from.place) else {
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
            .find(|pane| pane.id == id && pane.open())
            .cloned();
        match pane {
            Some(pane) => self.display(&program, from.place, pane, None),
            None => self.say(from.place, "that window has closed"),
        }
    }

    /// `ATTACH_COMMAND`, or `None` once `place` is told it is not set.
    fn program(&self, place: Place) -> Option<PathBuf> {
        let program = self.attach.clone();
        if program.is_none() {
            self.say(place, "`ATTACH_COMMAND` is not set");
        }
        program
    }

    /// Runs `program` with `pane` and its tmux server's socket as its arguments,
    /// acknowledging the message `asked` when one asked. The program reaches the other
    /// device over the network, so it runs on a thread of its own.
    fn display(&self, program: &Path, place: Place, pane: Pane, asked: Option<i64>) {
        let telegram = Arc::clone(&self.telegram);
        let program = program.to_owned();
        std::thread::spawn(move || {
            match process::run(Command::new(&program).arg(&pane.id).arg(&pane.server)) {
                Ok(_) => {
                    if let Some(asked) = asked {
                        telegram.acknowledge(place.chat, asked);
                    }
                }
                Err(error) => {
                    let said = format!("`ATTACH_COMMAND` failed\n\n```\n{error}\n```");
                    telegram.send(place, &said, Sound::Silent, asked);
                }
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

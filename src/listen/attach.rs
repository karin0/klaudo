//! A session's terminal, shown on another device by a program the env file names.

use std::process::Command;
use std::sync::Arc;

use kuriero::Message;

use crate::process;
use crate::telegram::{Place, Sound};
use crate::tmux::Pane;

use super::chat::{NEW, NOTHING_RAN, unseen};
use super::{Awaiting, Machine};

impl Machine {
    /// Runs `ATTACH_COMMAND` with the pane of the session a message replying to
    /// `replied` would reach and its tmux server's socket as its arguments. A session that has exited is
    /// resumed in a window of its own first, which shows it starting. The command reaches
    /// the other device over the network, so it runs on a thread of its own.
    pub(super) fn attach(&mut self, place: Place, asked: i64, replied: Option<&Message>) {
        let Some(program) = self.attach.clone() else {
            return self.say(place, "`ATTACH_COMMAND` is not set");
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
        let Some(pane) = self.pane(place, &address) else {
            return;
        };
        let telegram = Arc::clone(&self.telegram);
        std::thread::spawn(move || {
            match process::run(Command::new(&program).arg(&pane.id).arg(&pane.server)) {
                Ok(_) => telegram.acknowledge(place.chat, asked),
                Err(error) => {
                    let said = format!("`ATTACH_COMMAND` failed\n\n```\n{error}\n```");
                    telegram.send(place, &said, Sound::Silent, Some(asked));
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
        self.open(dir, Some(id), Awaiting::Terminal(place))
    }
}

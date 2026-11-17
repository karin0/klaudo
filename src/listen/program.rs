//! A program the env file names for one of the daemon's commands.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use crate::process;
use crate::telegram::{Place, Sound, Telegram};

pub(super) struct Program {
    /// The variable of the env file naming the program.
    variable: &'static str,
    path: Option<PathBuf>,
}

impl Program {
    pub(super) fn new(variable: &'static str) -> Self {
        Self {
            variable,
            path: crate::telegram::setting(variable).map(PathBuf::from),
        }
    }

    pub(super) fn is_set(&self) -> bool {
        self.path.is_some()
    }

    /// The program to run, or `None` once `place` is told its variable is not set.
    pub(super) fn command(&self, telegram: &Telegram, place: Place) -> Option<Command> {
        let command = self.path.as_ref().map(Command::new);
        if command.is_none() {
            let said = format!("`{}` is not set", self.variable);
            telegram.send(place, &said, Sound::Silent, None);
        }
        command
    }

    /// Runs `command` and hands its output to `done`, or posts in `place` how it failed,
    /// replying to `asked`. The program may reach over the network, so it runs on a
    /// thread of its own.
    pub(super) fn spawn(
        &self,
        telegram: &Arc<Telegram>,
        mut command: Command,
        place: Place,
        asked: Option<i64>,
        done: impl FnOnce(&Telegram, Vec<u8>) + Send + 'static,
    ) {
        let telegram = Arc::clone(telegram);
        let variable = self.variable;
        std::thread::spawn(move || match process::run(&mut command) {
            Ok(output) => done(&telegram, output),
            Err(error) => {
                let said = format!("`{variable}` failed\n\n```\n{error}\n```");
                telegram.send(place, &said, Sound::Silent, asked);
            }
        });
    }
}

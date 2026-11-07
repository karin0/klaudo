//! A conversation copied up to the start of one of its turns, which `/rewind` makes.

use kuriero::Message;

use crate::hook;
use crate::telegram::{Admitted, Sound};

use super::chat::{NEW, NOTHING_RAN, turn, unseen};
use super::render::code;
use super::{Known, Last, Machine, Trail, transcript};

impl Machine {
    /// Copies the conversation of the session a message replying to `replied` would
    /// reach up to the start of the turn `replied` belongs to, or else of the first turn
    /// after its latest compaction, into a session that has not run. The copy's head goes
    /// where the message was sent, over the prompt of that turn, as Claude Code's own
    /// `/rewind` hands the prompt back. A reply to it resumes the copy there, and so does
    /// a message replying to nothing, since the copy left the latest message there.
    pub(super) fn rewind(&mut self, from: &Admitted, asked: i64, replied: Option<&Message>) {
        let place = from.place;
        let address = match self.addressee(place, replied) {
            Ok(Some(address)) if address != NEW => address,
            Ok(_) => return self.say(place, NOTHING_RAN),
            Err(error) => return self.say(place, error),
        };
        let found = self
            .sessions
            .iter()
            .find(|(id, _)| id.starts_with(&address))
            .map(|(id, session)| (id.clone(), session.dir.clone(), session.transcript_path()))
            .or_else(|| {
                self.ended
                    .iter()
                    .find(|ended| ended.id.starts_with(&address))
                    .map(|ended| {
                        (
                            ended.id.clone(),
                            ended.dir.clone(),
                            ended.transcript.clone(),
                        )
                    })
            });
        let Some((id, dir, path)) = found else {
            return self.say(place, &unseen(&address));
        };
        if !self.reaches(from.guest.as_deref(), place, &dir) {
            return;
        }
        let Some(path) = path else {
            return self.say(place, "the session named no transcript to rewind");
        };
        let turn = replied.and_then(turn);
        let rewound = match transcript::rewind(&path, turn.as_deref()) {
            Ok(Some(rewound)) => rewound,
            Ok(None) if turn.is_some() => {
                return self.say(
                    place,
                    "that turn is not in the session's transcript, or nothing precedes it",
                );
            }
            Ok(None) => {
                return self.say(place, "that conversation has no prompt after a compaction");
            }
            Err(error) => {
                let error = format!("{}: {error}", path.display());
                return self.say(place, &hook::prose(&error));
            }
        };
        let body = format!(
            "rewound {} to before\n\n{}",
            code(&hook::address(&id, None)),
            hook::quote(&rewound.prompt),
        );
        let post = hook::compose(&hook::Head::new(&dir, &rewound.id, None), "", "", &body);
        let message = self.telegram.post(place, &post, Sound::Silent, Some(asked));
        let transcript = path.with_file_name(format!("{}.jsonl", rewound.id));
        self.exited(Known {
            id: rewound.id,
            dir,
            trail: Trail {
                last: Some(Last::now(place, message)),
                ..Trail::default()
            },
            transcript: Some(transcript),
        });
    }
}

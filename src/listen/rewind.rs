//! A conversation copied as it stood after its latest compaction, which `/rewind` makes.

use kuriero::Message;

use crate::hook;
use crate::telegram::{Admitted, Sound};

use super::chat::{NEW, NOTHING_RAN, unseen};
use super::render::code;
use super::{Known, Last, Machine, Trail, transcript};

impl Machine {
    /// Copies the conversation of the session a message replying to `replied` would
    /// reach, as it stood after its latest compaction, into a session that has not run,
    /// and posts the copy's head where the message was sent. A reply to that message
    /// resumes the copy there, and so does a message replying to nothing, since the copy
    /// left the latest message there.
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
        let copy = match transcript::rewind(&path) {
            Ok(Some(copy)) => copy,
            Ok(None) => {
                return self.say(place, "that conversation has no prompt after a compaction");
            }
            Err(error) => {
                let error = format!("{}: {error}", path.display());
                return self.say(place, &hook::prose(&error));
            }
        };
        let said = format!(
            "{}\nrewound {} to its latest compaction",
            hook::Head::new(&dir, &copy, None).markdown(),
            code(&hook::address(&id, None)),
        );
        let message = self.telegram.send(place, &said, Sound::Silent, Some(asked));
        let transcript = path.with_file_name(format!("{copy}.jsonl"));
        self.exited(Known {
            id: copy,
            dir,
            trail: Trail {
                last: Some(Last::now(place, message)),
                ..Trail::default()
            },
            transcript: Some(transcript),
        });
    }
}

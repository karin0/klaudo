//! `/diff`: a link to the unpushed changes of a working tree, which the review page
//! shows and the conversation a message would reach takes the review of.

use std::path::Path;

use kuriero::Message;

use crate::changes;
use crate::hook;
use crate::telegram::{Admitted, Sound};
use crate::web::Links;

use super::Machine;
use super::chat::{NEW, expand, tilde};
use super::render::code;
use super::turn::review;

impl Machine {
    /// Posts under the head of the session a message replying to `replied` would reach,
    /// so a reply reaches it too, the link to the changes since its upstream of the
    /// working tree holding `argument`, or the session's directory without one.
    pub(super) fn diff(
        &self,
        from: &Admitted,
        asked: i64,
        replied: Option<&Message>,
        argument: &str,
    ) {
        let place = from.place;
        let address = match self.addressee(place, replied) {
            Ok(address) => address.filter(|address| address != NEW),
            Err(error) => return self.say(place, error),
        };
        let Some((id, dir, _)) =
            address.and_then(|address| self.known().find(|(id, _, _)| id.starts_with(&address)))
        else {
            return self.say(place, "no session has run here to show the changes of");
        };
        let held = if argument.is_empty() {
            dir.to_owned()
        } else if let Some(held) = expand(argument) {
            held
        } else {
            return self.say(place, &format!("{} is not a directory", code(argument)));
        };
        if !self.reaches(from.guest.as_deref(), place, &held) {
            return;
        }
        let Some(links) = Links::new(self.app, id) else {
            return self.say(place, "this daemon serves no review page; set `WEB_LISTEN`");
        };
        let line = changes::top(&held).and_then(|top| {
            let since = changes::pushed(&top)?;
            let path = if argument.is_empty() {
                Path::new(".")
            } else {
                &held
            };
            let line = review(links, &top, &since, &path.display().to_string())?;
            let at = code(&tilde(&top));
            Ok(line.map_or_else(
                || format!("{at} has no unpushed changes"),
                |line| format!("{line} in {at}"),
            ))
        });
        let body = line.unwrap_or_else(|error| hook::prose(&error));
        let said = hook::compose(&hook::Head::new(dir, id, None), "", "", &body).markdown;
        self.telegram.send(place, &said, Sound::Silent, Some(asked));
    }
}

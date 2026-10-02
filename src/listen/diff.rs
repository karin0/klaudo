//! `/diff`: the unstaged changes of the conversation a message would reach, drawn.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kuriero::Message;

use crate::diff::{self, CHANGED_MAX, Change};
use crate::hook;
use crate::telegram::{Place, Sound, Telegram};

use super::chat::NEW;
use super::render::code;
use super::{Machine, runtime_dir};

/// The most photos a rich message holds.
const PHOTOS_MAX: usize = 50;

impl Machine {
    /// The changes of the session a message replying to `replied` would reach, posted
    /// under its head, so a reply to them reaches it too. Drawing and uploading take
    /// seconds, so they run on a thread of their own while the hooks go on arriving.
    pub(super) fn diff(&self, place: Place, asked: i64, replied: Option<&Message>) {
        let address = match self.addressee(place, replied) {
            Ok(address) => address.filter(|address| address != NEW),
            Err(error) => return self.say(place, error),
        };
        let Some((id, dir, _)) =
            address.and_then(|address| self.known().find(|(id, _, _)| id.starts_with(&address)))
        else {
            return self.say(place, "no session has run here to show the changes of");
        };
        let head = hook::Head::new(dir, id, None);
        let dir = dir.to_owned();
        let scratch = runtime_dir()
            .expect("XDG_RUNTIME_DIR")
            .join(format!("diff-{}-{asked}", place.chat));
        let telegram = Arc::clone(&self.telegram);
        std::thread::spawn(move || {
            if let Err(error) = post(&telegram, place, asked, &head, &dir, &scratch) {
                let said = hook::compose(&head, "", "", &hook::prose(&error)).markdown;
                telegram.send(place, &said, Sound::Silent, Some(asked));
            }
            if let Err(error) = std::fs::remove_dir_all(&scratch)
                && error.kind() != ErrorKind::NotFound
            {
                eprintln!("{}: {error}", scratch.display());
            }
        });
    }
}

/// Each file folded under a line of what changed in it, its pictures inside, or that
/// line alone where it is binary, too large to draw, or past what the message holds.
fn post(
    telegram: &Telegram,
    place: Place,
    asked: i64,
    head: &hook::Head,
    dir: &Path,
    scratch: &Path,
) -> Result<(), String> {
    let changes = diff::changes(dir)?;
    std::fs::create_dir_all(scratch).map_err(|error| format!("{}: {error}", scratch.display()))?;
    let mut photos: Vec<PathBuf> = Vec::new();
    let mut blocks = Vec::new();
    for change in &changes {
        let pages = change.pages()?;
        let summary = summary(change);
        if pages.is_empty() || photos.len() + pages.len() > PHOTOS_MAX {
            let why = if change.added + change.removed > CHANGED_MAX {
                format!(", over the {CHANGED_MAX} lines drawn")
            } else if pages.is_empty() {
                String::new()
            } else {
                format!(", past the {PHOTOS_MAX} photos a message holds")
            };
            blocks.push(format!("{summary}{}", hook::prose(&why)));
            continue;
        }
        let mut pictures = Vec::new();
        for page in &pages {
            let png = scratch.join(format!("p{}.png", photos.len()));
            diff::draw(page, &png)?;
            pictures.push(format!("![](tg://photo?id=p{})", photos.len()));
            photos.push(png);
        }
        blocks.push(format!(
            "<details><summary>{summary}</summary>\n\n{}\n\n</details>",
            pictures.join("\n\n")
        ));
    }
    if blocks.is_empty() {
        blocks.push("no unstaged changes".to_owned());
    }
    let markdown = hook::compose(head, "", "", &blocks.join("\n\n")).markdown;
    if photos.is_empty() {
        telegram.send(place, &markdown, Sound::Silent, Some(asked));
    } else {
        telegram.photos(place, &markdown, &photos, asked);
    }
    Ok(())
}

/// The status, the path in a code span, since Telegram reads a name ending in `.md` or
/// `.rs` as a link, and the line counts.
fn summary(change: &Change) -> String {
    let path = match &change.from {
        Some(from) => format!("{} → {}", code(from), code(&change.path)),
        None => code(&change.path),
    };
    let counts = if change.binary() {
        "binary".to_owned()
    } else {
        hook::prose(&format!("+{} −{}", change.added, change.removed))
    };
    format!("{} {path} {counts}", change.status)
}

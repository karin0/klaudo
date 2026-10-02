//! Delivering text to a session's terminal. `send-keys` reaches the input box of the
//! TUI, so what arrives is a prompt the user typed rather than a message from a peer.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// The tmux session that holds the windows the daemon opens for conversations started
/// from the chat, so one `tmux attach -t klaudo` reaches all of them.
const OWNED_SESSION: &str = "klaudo";
/// Named rather than the default buffer, so a paste the daemon issues cannot consume
/// what the user copied.
const BUFFER: &str = "klaudo";
/// The kernel registers all 2^20 pseudo-terminals under this one major, so a minor is
/// the number under `/dev/pts`.
const PTS_MAJOR: u32 = 136;
/// Claude Code folds a paste of four lines or of about 800 UTF-16 units into a
/// `[Pasted text #N]` placeholder and submits it wrapped as content the user did not
/// write, so a message goes in pieces below both, which each stay typed text.
const PIECE_NEWLINES: usize = 2;
const PIECE_UNITS: usize = 700;

/// Where a session's terminal is. `$TMUX` names the server, `$TMUX_PANE` the pane, and
/// a hook inherits both from the session it reports for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub server: String,
    pub id: String,
}

impl Pane {
    /// The socket path is the first field of `$TMUX`, which is what addresses a server
    /// started under any `-L` or `-S` name.
    pub fn new(tmux: &str, pane: &str) -> Self {
        let server = tmux.split(',').next().unwrap_or(tmux).to_owned();
        Self {
            server,
            id: pane.to_owned(),
        }
    }

    fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command.arg("-S").arg(&self.server);
        command
    }

    fn display(&self, format: &str) -> Option<String> {
        let output = self
            .tmux()
            .args(["display-message", "-p", "-t", &self.id, format])
            .output()
            .ok()?;
        let shown = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (output.status.success() && !shown.is_empty()).then_some(shown)
    }

    /// The terminal this pane is showing, which is what ties it to a process.
    fn tty(&self) -> Option<String> {
        self.display("#{pane_tty}")
    }

    /// True for a pane in one of the windows `open` makes.
    pub fn owned(&self) -> bool {
        self.display("#{session_name}").as_deref() == Some(OWNED_SESSION)
    }

    pub fn close(&self) -> Result<(), String> {
        run(self.tmux().args(["kill-pane", "-t", &self.id]))
    }

    /// False once the pane has closed, as a window does when its command exits.
    pub fn open(&self) -> bool {
        self.tty().is_some()
    }

    /// True while this pane is still showing the session that claimed it. A session
    /// that exited leaves its pane to a shell, where the same text would run as a
    /// command, so this is checked before every delivery.
    pub fn holds(&self, pid: u32) -> bool {
        controlling_tty(pid).is_some_and(|tty| self.tty() == Some(tty))
    }

    /// Text arrives through a paste buffer, so newlines reach the input box as newlines
    /// and nothing in the message needs escaping. The Enter that follows submits it.
    pub fn deliver(&self, text: &str) -> Result<(), String> {
        // A pane in copy mode, which a wheel tick alone enters wherever the mouse is
        // on, takes the paste into the input box and gives the Enter to the mode's own
        // key table, where it ends the mode and submits nothing. Leaving the mode is a
        // no-op on a pane that is in none.
        run(self.tmux().args(["copy-mode", "-q", "-t", &self.id]))?;
        for piece in pieces(text) {
            self.paste(piece)?;
        }
        run(self.tmux().args(["send-keys", "-t", &self.id, "Enter"]))
    }

    /// Escape interrupts the turn a session is running. Copy mode would take it to end
    /// the mode, so the mode is left first, as for a delivery.
    pub fn interrupt(&self) -> Result<(), String> {
        run(self.tmux().args(["copy-mode", "-q", "-t", &self.id]))?;
        run(self.tmux().args(["send-keys", "-t", &self.id, "Escape"]))
    }

    fn paste(&self, text: &str) -> Result<(), String> {
        let mut load = self
            .tmux()
            .args(["load-buffer", "-b", BUFFER, "-"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|error| format!("load-buffer: {error}"))?;
        load.stdin
            .take()
            .expect("piped")
            .write_all(text.as_bytes())
            .map_err(|error| format!("load-buffer: {error}"))?;
        let loaded = load
            .wait()
            .map_err(|error| format!("load-buffer: {error}"))?;
        if !loaded.success() {
            return Err(format!("load-buffer exited {loaded}"));
        }
        run(self
            .tmux()
            .args(["paste-buffer", "-b", BUFFER, "-t", &self.id, "-p", "-d"]))
    }

    /// What the pane is showing, for reporting a session that stopped where the daemon
    /// cannot type, such as the dialog that asks whether a folder is trusted.
    pub fn screen(&self) -> Option<String> {
        let output = self
            .tmux()
            .args(["capture-pane", "-p", "-S", "-40", "-t", &self.id])
            .output()
            .ok()?;
        let screen = String::from_utf8_lossy(&output.stdout);
        let kept: Vec<&str> = screen
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        (!kept.is_empty()).then(|| kept.join("\n"))
    }
}

/// Opens a window running a session in `cwd`, the one `resume` names or a new one, and
/// returns its pane. That session's own `SessionStart` says it is ready to type into,
/// so nothing here waits for it.
pub fn open(cwd: &Path, resume: Option<&str>) -> Result<Pane, String> {
    let exact = format!("={OWNED_SESSION}");
    // `new-session -A` attaches to a session already there even with `-d`, and this
    // process has no terminal to attach with, so the first window opens the session.
    let present = Command::new("tmux")
        .args(["has-session", "-t", &exact])
        .output()
        .is_ok_and(|output| output.status.success());
    let mut command = Command::new("tmux");
    if present {
        command.args(["new-window", "-t", &format!("{exact}:")]);
    } else {
        command.args(["new-session", "-d", "-s", OWNED_SESSION]);
    }
    let directory = cwd.to_str().ok_or("cwd is not utf-8")?;
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("claude");
    let output = command
        .args(["-P", "-F", "#{pane_id} #{socket_path}"])
        .args(["-c", directory, "-n", name, "claude"])
        .args(resume.map(|id| ["--resume", id]).into_iter().flatten())
        .output()
        .map_err(|error| format!("tmux: {error}"))?;
    let printed = String::from_utf8_lossy(&output.stdout);
    match printed.trim_end().split_once(' ') {
        Some((pane, server)) if output.status.success() => Ok(Pane::new(server, pane)),
        _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
    }
}

fn pieces(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let (mut start, mut newlines, mut units) = (0, 0, 0);
    for (index, char) in text.char_indices() {
        let newline = usize::from(char == '\n');
        if newlines + newline > PIECE_NEWLINES || units + char.len_utf16() > PIECE_UNITS {
            pieces.push(&text[start..index]);
            (start, newlines, units) = (index, 0, 0);
        }
        newlines += newline;
        units += char.len_utf16();
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

fn run(command: &mut Command) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|error| format!("{}: {error}", command.get_program().display()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

/// The fields of `/proc/<pid>/stat` from the third on, past the command name, which may
/// itself contain spaces and parentheses.
fn stat(pid: &str) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(
        stat.rsplit_once(") ")?
            .1
            .split(' ')
            .map(str::to_owned)
            .collect(),
    )
}

/// Field 7 of `/proc/<pid>/stat` is the controlling terminal, encoded as a device
/// number.
pub fn controlling_tty(pid: u32) -> Option<String> {
    pts(stat(&pid.to_string())?.get(4)?.parse().ok()?)
}

/// True while a process has `pid` as its parent, field 4 of its `/proc/<pid>/stat`.
pub fn has_children(pid: u32) -> bool {
    let parent = pid.to_string();
    std::fs::read_dir("/proc")
        .expect("/proc")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| stat(&name))?
        })
        .any(|fields| fields.get(1) == Some(&parent))
}

/// A tmux pane is always a pseudo-terminal, so any other terminal names none.
fn pts(device: libc::dev_t) -> Option<String> {
    (libc::major(device) == PTS_MAJOR).then(|| format!("/dev/pts/{}", libc::minor(device)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_takes_its_server_from_the_first_field_of_tmux() {
        let pane = Pane::new("/tmp/tmux-1000/klaudoprobe,153856,0", "%0");
        assert_eq!(pane.server, "/tmp/tmux-1000/klaudoprobe");
        assert_eq!(pane.id, "%0");
    }

    #[test]
    fn this_process_reports_the_terminal_it_was_started_from() {
        let own = controlling_tty(std::process::id());
        assert!(
            own.as_ref().is_none_or(|tty| tty.starts_with("/dev/pts/")),
            "controlling terminal reads {own:?}"
        );
    }

    #[test]
    fn a_device_number_decodes_past_the_first_256_terminals() {
        // The encoding `/proc` uses: the minor's low byte, the major, then the minor's
        // remaining bits.
        let encode =
            |major: u64, minor: u64| (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
        assert_eq!(pts(encode(136, 5)).as_deref(), Some("/dev/pts/5"));
        assert_eq!(pts(encode(136, 300)).as_deref(), Some("/dev/pts/300"));
        // The first virtual console.
        assert_eq!(pts(encode(4, 1)), None);
    }

    #[test]
    fn a_message_goes_in_pieces_short_enough_to_stay_typed() {
        let text = format!("one\ntwo\nthree\nfour\n{}", "猫".repeat(PIECE_UNITS + 1));
        let cut = pieces(&text);
        assert_eq!(cut.concat(), text);
        assert_eq!(cut[0], "one\ntwo\nthree");
        assert_eq!(cut.len(), 3);
        assert!(cut.iter().all(|piece| {
            piece.matches('\n').count() <= PIECE_NEWLINES
                && piece.encode_utf16().count() <= PIECE_UNITS
        }));
        assert!(pieces("").is_empty());
    }

    #[test]
    fn a_pane_that_names_no_server_refuses_to_hold_anything() {
        let pane = Pane::new("/nonexistent/tmux-socket", "%0");
        assert!(!pane.holds(std::process::id()));
    }

    /// The pane reads one line and writes it back, so what it shows says the Enter
    /// submitted rather than only that the text arrived.
    #[test]
    fn a_reply_submits_in_a_pane_the_reader_left_in_copy_mode() {
        let socket = std::env::temp_dir().join(format!("klaudo-test-{}", std::process::id()));
        let tmux = |args: &[&str]| {
            let output = Command::new("tmux")
                .arg("-S")
                .arg(&socket)
                .args(args)
                .output()
                .expect("tmux runs");
            assert!(output.status.success(), "tmux {args:?}: {output:?}");
            String::from_utf8(output.stdout)
                .expect("utf-8")
                .trim()
                .to_owned()
        };
        tmux(&[
            "new-session",
            "-d",
            "-s",
            "probe",
            "sh",
            "-c",
            "read line; printf 'read %s' \"$line\"; sleep 30",
        ]);
        let id = tmux(&["display-message", "-p", "-t", "probe", "#{pane_id}"]);
        tmux(&["copy-mode", "-t", "probe"]);
        let pane = Pane::new(&socket.to_string_lossy(), &id);

        let delivered = pane.deliver("scrolled away");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut screen = String::new();
        while std::time::Instant::now() < deadline {
            screen = pane.screen().unwrap_or_default();
            if screen.contains("read scrolled away") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        tmux(&["kill-server"]);
        delivered.expect("delivered");
        assert!(
            screen.contains("read scrolled away"),
            "screen reads {screen:?}"
        );
    }
}

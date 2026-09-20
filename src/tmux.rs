//! Delivering text to a session's terminal. `send-keys` reaches the input box of the
//! TUI, so what arrives is a prompt the user typed rather than a message from a peer.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The tmux session that holds the windows klaude opens for conversations started from
/// the chat, so one `tmux attach -t klaude` reaches all of them.
const OWNED_SESSION: &str = "klaude";
/// Named rather than the default buffer, so a paste klaude issues cannot consume what
/// the user copied.
const BUFFER: &str = "klaude";
/// The input box takes a paste on the session's own tick, and an Enter that reaches it
/// in the same read is handled while the box is still empty. tmux writes both into one
/// buffer for the pane, which libevent is free to flush in a single write, so the wait
/// is what keeps them apart. Claude Code waits 10 milliseconds here when it types a
/// reply into a session itself; this goes through a tmux server and two process starts,
/// which jitter by more than that.
const SETTLE: Duration = Duration::from_millis(100);

/// Where a session's terminal is. `$TMUX` names the server, `$TMUX_PANE` the pane, and
/// a hook inherits both from the session it reports for.
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// The terminal this pane is showing, which is what ties it to a process.
    fn tty(&self) -> Option<String> {
        let output = self
            .tmux()
            .args(["display-message", "-p", "-t", &self.id, "#{pane_tty}"])
            .output()
            .ok()?;
        let tty = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (output.status.success() && !tty.is_empty()).then_some(tty)
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
            .args(["paste-buffer", "-b", BUFFER, "-t", &self.id, "-p", "-d"]))?;
        std::thread::sleep(SETTLE);
        run(self.tmux().args(["send-keys", "-t", &self.id, "Enter"]))
    }

    /// What the pane is showing, for reporting a session that stopped where klaude
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

/// Opens a window running a session in `cwd`. The pane it lands in is learned from that
/// session's own `SessionStart`, which is also what says the session is ready to type
/// into, so nothing here waits for it.
pub fn open(cwd: &Path) -> Result<(), String> {
    // Attaches to the session when it is already there, and this process has no
    // terminal to attach with, hence detached.
    run(Command::new("tmux").args(["new-session", "-d", "-A", "-s", OWNED_SESSION]))?;
    let target = format!("{OWNED_SESSION}:");
    let directory = cwd.to_str().ok_or("cwd is not utf-8")?;
    run(Command::new("tmux").args([
        "new-window",
        "-t",
        &target,
        "-c",
        directory,
        "-n",
        cwd.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("claude"),
        "claude",
    ]))
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

/// Field 7 of `/proc/<pid>/stat` is the controlling terminal, encoded as a device
/// number. The fields before it are skipped past the command name, which may itself
/// contain spaces and parentheses.
pub fn controlling_tty(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(") ")?.1.split(' ').collect();
    let device: u32 = fields.get(4)?.parse().ok()?;
    Some(format!("/dev/pts/{}", device & 0xff))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_takes_its_server_from_the_first_field_of_tmux() {
        let pane = Pane::new("/tmp/tmux-1000/klaudeprobe,153856,0", "%0");
        assert_eq!(pane.server, "/tmp/tmux-1000/klaudeprobe");
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
    fn a_pane_that_names_no_server_refuses_to_hold_anything() {
        let pane = Pane::new("/nonexistent/tmux-socket", "%0");
        assert!(!pane.holds(std::process::id()));
    }

    /// The pane reads one line and writes it back, so what it shows says the Enter
    /// submitted rather than only that the text arrived.
    #[test]
    fn a_reply_submits_in_a_pane_the_reader_left_in_copy_mode() {
        let socket = std::env::temp_dir().join(format!("klaude-test-{}", std::process::id()));
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

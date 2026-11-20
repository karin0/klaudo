//! Other processes: running a program to its end within a deadline, and telling a
//! running one apart from a later one given its id.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);

/// What `command` printed, or what went wrong running it: its own complaint, or how
/// long it ran before it was killed.
pub fn run(command: &mut Command) -> Result<Vec<u8>, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut read = Vec::new();
            pipe.read_to_end(&mut read).map(|_| read)
        })
    };
    let stdout = drain(Box::new(child.stdout.take().expect("piped stdout")));
    let stderr = drain(Box::new(child.stderr.take().expect("piped stderr")));
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("{program}: {error}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program}: killed after {PROCESS_TIMEOUT:?}"));
        }
        std::thread::sleep(POLL);
    };
    let read = |pipe: std::thread::JoinHandle<std::io::Result<Vec<u8>>>| {
        pipe.join()
            .expect("a pipe reader")
            .map_err(|error| format!("{program}: {error}"))
    };
    let (stdout, stderr) = (read(stdout)?, read(stderr)?);
    let complaint = String::from_utf8_lossy(&stderr);
    match complaint.trim() {
        _ if status.success() => Ok(stdout),
        "" => Err(format!("{program}: {status}")),
        complaint => Err(format!("{program}: {complaint}")),
    }
}

/// A process with the time it started, since a process started later, after it exited
/// or across a reboot, can be given its id.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Process {
    pub pid: u32,
    /// Field 22 of `/proc/<pid>/stat`, in clock ticks since the machine booted.
    pub started: u64,
}

impl Process {
    pub fn of(pid: u32) -> Option<Self> {
        let started = stat(&pid.to_string())?.get(19)?.parse().ok()?;
        Some(Self { pid, started })
    }

    pub fn running(self) -> bool {
        Self::of(self.pid) == Some(self)
    }
}

/// The fields of `/proc/<pid>/stat` from the third on, past the command name, which may
/// itself contain spaces and parentheses.
pub fn stat(pid: &str) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(
        stat.rsplit_once(") ")?
            .1
            .split(' ')
            .map(str::to_owned)
            .collect(),
    )
}

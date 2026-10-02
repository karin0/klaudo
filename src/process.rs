//! Running a program to its end, within a deadline.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

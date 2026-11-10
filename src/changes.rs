//! A working tree's changes since a commit, which the review page shows, read through
//! git's plumbing. `git diff` would rewrite the index to refresh its record of a file
//! whose modification time changed, and a session's own git command fails while the
//! index is locked for that. Every command spells out the output options it parses, so
//! a user's git configuration leaves the output as read here.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::process::run;

/// The commits the page lists from `HEAD`, enough to move the start a few turns back.
const COMMITS_MAX: usize = 30;
/// An untracked file past this is shown as binary, as a phone shows it too slowly.
const UNTRACKED_MAX: u64 = 1 << 20;

/// Where a turn starts: the top of the working tree its session runs in, and the commit
/// checked out there.
pub struct Start {
    pub top: PathBuf,
    pub head: String,
}

/// How much changed since a start, as its link counts it.
#[derive(Debug, PartialEq)]
pub struct Summary {
    pub commits: usize,
    pub files: usize,
}

/// A commit of the list the page moves the start along.
#[derive(Serialize)]
pub struct Commit {
    pub hash: String,
    /// The first parent, `None` for a root commit.
    pub parent: Option<String>,
    /// In Unix seconds.
    pub time: u64,
    pub subject: String,
    /// Whether the changes since the start include it.
    pub included: bool,
}

/// What the page shows of the changes since `since`.
#[derive(Serialize)]
pub struct Changes {
    pub since: String,
    pub commits: Vec<Commit>,
    /// The changes as a git patch, untracked files as new ones.
    pub patch: String,
}

fn git(top: &Path) -> Command {
    let mut git = Command::new("git");
    git.arg("-C").arg(top).args(["-c", "core.quotePath=false"]);
    git
}

fn text(output: &[u8]) -> String {
    String::from_utf8_lossy(output).into_owned()
}

/// The start of a turn in `dir`, `None` outside a working tree or before its first
/// commit.
pub fn start(dir: &Path) -> Option<Start> {
    let mut git = Command::new("git");
    git.arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel", "HEAD"]);
    let output = text(&run(&mut git).ok()?);
    let mut lines = output.lines();
    let top = PathBuf::from(lines.next()?);
    let head = lines.next()?.to_owned();
    Some(Start { top, head })
}

/// The top of the working tree holding `dir`.
pub fn top(dir: &Path) -> Result<PathBuf, String> {
    let mut git = Command::new("git");
    git.arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"]);
    Ok(PathBuf::from(text(&run(&mut git)?).trim_end()))
}

/// The commit the unpushed changes of the working tree at `top` start from: where `HEAD`
/// forked from its upstream, or `HEAD` without one.
pub fn pushed(top: &Path) -> Result<String, String> {
    let forked = run(git(top).args(["merge-base", "HEAD", "@{upstream}"]));
    let hash = forked.or_else(|_| {
        run(git(top).args(["rev-parse", "--verify", "--quiet", "HEAD"]))
            .map_err(|_| "no commit yet".to_owned())
    })?;
    Ok(text(&hash).trim_end().to_owned())
}

/// Whether `text` is a full commit hash, in SHA-1 or SHA-256.
pub fn hash(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Refuses a start that is no full commit hash of the working tree at `top`. Only hex
/// digits pass, so git cannot read a start as an option.
pub fn verify(top: &Path, since: &str) -> Result<(), String> {
    if !hash(since) {
        return Err(format!("{since} is no commit hash"));
    }
    run(git(top).args([
        "rev-parse",
        "--quiet",
        "--verify",
        &format!("{since}^{{commit}}"),
    ]))
    .map(drop)
    .map_err(|_| format!("{since} is no commit of this repository"))
}

/// The commits and files changed since `since`, which `Stop` counts for its link.
pub fn summary(top: &Path, since: &str) -> Result<Summary, String> {
    let commits = text(&run(git(top).args([
        "rev-list",
        "--count",
        "--first-parent",
        &format!("{since}..HEAD"),
    ]))?)
    .trim()
    .parse()
    .map_err(|error| format!("git rev-list --count: {error}"))?;
    // `--numstat` reads the content of a file whose modification time alone changed,
    // where `--name-only` would count it.
    let changed = run(git(top).args(["diff-index", "--numstat", "-z", "-M", since]))?;
    let changed = changed
        .split(|byte| *byte == 0)
        .filter(|field| field.contains(&b'\t'))
        .count();
    Ok(Summary {
        commits,
        files: changed + untracked(top)?.len(),
    })
}

/// Everything the page shows of the changes since `since`, which `verify` passed.
pub fn changes(top: &Path, since: &str) -> Result<Changes, String> {
    let mut patch = text(&run(git(top).args([
        "diff-index",
        "--patch",
        "--no-color",
        "--no-ext-diff",
        "-M",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        since,
    ]))?);
    for path in untracked(top)? {
        added(&mut patch, &path, &top.join(&path));
    }
    Ok(Changes {
        since: since.to_owned(),
        commits: commits(top, since)?,
        patch,
    })
}

/// The file at `path` as of `since`.
pub fn old(top: &Path, since: &str, path: &str) -> Result<Vec<u8>, String> {
    run(git(top).args(["show", &format!("{since}:{path}")]))
}

/// The files git neither tracks nor ignores, relative to `top`.
fn untracked(top: &Path) -> Result<Vec<String>, String> {
    let listed = text(&run(git(top).args([
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
    ]))?);
    Ok(listed
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The first-parent history from `HEAD`, each commit marked by whether the changes
/// since `since` include it.
fn commits(top: &Path, since: &str) -> Result<Vec<Commit>, String> {
    let max = format!("--max-count={COMMITS_MAX}");
    let included = text(&run(git(top).args([
        "rev-list",
        "--first-parent",
        &max,
        &format!("{since}..HEAD"),
    ]))?);
    let log = text(&run(git(top).args([
        "log",
        "--first-parent",
        "--no-color",
        &max,
        "--date=unix",
        "--format=%H%x00%P%x00%cd%x00%s%x00",
        "HEAD",
    ]))?);
    let mut fields = log.split('\0').map(|field| field.trim_start_matches('\n'));
    let mut commits = Vec::new();
    while let (Some(hash), Some(parents), Some(time), Some(subject)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    {
        commits.push(Commit {
            hash: hash.to_owned(),
            parent: parents
                .split(' ')
                .next()
                .filter(|parent| !parent.is_empty())
                .map(str::to_owned),
            time: time.parse().unwrap_or_default(),
            subject: subject.to_owned(),
            included: included.lines().any(|line| line == hash),
        });
    }
    Ok(commits)
}

/// Appends to `patch` the untracked file `name`, read from `file`, as git writes a new
/// file: its lines, or a line calling it binary when it holds no text in UTF-8 or is too
/// large to show.
fn added(patch: &mut String, name: &str, file: &Path) {
    let content = fs::metadata(file)
        .ok()
        .filter(|metadata| metadata.len() <= UNTRACKED_MAX)
        .and_then(|_| fs::read(file).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|content| !content.contains('\0'));
    let a = quoted(&format!("a/{name}"));
    let b = quoted(&format!("b/{name}"));
    write!(patch, "diff --git {a} {b}\nnew file mode 100644\n").expect("a String takes writes");
    let Some(content) = content else {
        writeln!(patch, "Binary files /dev/null and {b} differ").expect("a String takes writes");
        return;
    };
    if content.is_empty() {
        return;
    }
    let lines: Vec<&str> = content
        .strip_suffix('\n')
        .unwrap_or(&content)
        .split('\n')
        .collect();
    write!(
        patch,
        "--- /dev/null\n+++ {b}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    )
    .expect("a String takes writes");
    for line in &lines {
        writeln!(patch, "+{line}").expect("a String takes writes");
    }
    if !content.ends_with('\n') {
        patch.push_str("\\ No newline at end of file\n");
    }
}

/// `path` as git writes it in a patch: in double quotes, with C escapes, when it holds a
/// quote, a backslash or a control character.
fn quoted(path: &str) -> String {
    if !path
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return path.to_owned();
    }
    let mut quoted = String::from("\"");
    for c in path.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            c if c.is_control() => {
                write!(quoted, "\\{:03o}", u32::from(c)).expect("a String takes writes");
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository at `dir` with `a` committed twice, the first commit's hash and the
    /// second's.
    fn repository(dir: &Path) -> (String, String) {
        let git = |args: &[&str]| {
            text(
                &run(Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                    .args(args))
                .expect("git runs"),
            )
        };
        git(&["init", "-q"]);
        fs::write(dir.join("a"), "1\n2\n3\n").expect("a file");
        fs::write(dir.join(".gitignore"), "ignored\n").expect("a file");
        git(&["add", "."]);
        git(&["commit", "-qm", "first"]);
        let first = git(&["rev-parse", "HEAD"]).trim().to_owned();
        fs::write(dir.join("a"), "1\ntwo\n3\n").expect("a file");
        git(&["commit", "-qam", "second"]);
        let second = git(&["rev-parse", "HEAD"]).trim().to_owned();
        (first, second)
    }

    #[test]
    fn unpushed_changes_start_where_head_forked_from_its_upstream() {
        let temporary = tempfile::tempdir().expect("a directory");
        let dir = temporary.path();
        let git = |args: &[&str]| run(git(dir).args(args)).expect("git runs");
        git(&["init", "-q"]);
        assert_eq!(pushed(dir), Err("no commit yet".to_owned()));
        let (first, second) = repository(dir);
        assert_eq!(pushed(dir), Ok(second));
        git(&["branch", "-q", "base", &first]);
        git(&["branch", "-q", "--set-upstream-to=base"]);
        assert_eq!(pushed(dir).as_ref(), Ok(&first));
        // An upstream that moved on keeps the start at the fork.
        git(&["switch", "-q", "base"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "third",
        ]);
        git(&["switch", "-q", "-"]);
        assert_eq!(pushed(dir), Ok(first));
    }

    #[test]
    fn changes_since_a_commit_take_in_its_successors_and_untracked_files() {
        let temporary = tempfile::tempdir().expect("a directory");
        let dir = temporary.path();
        let (first, second) = repository(dir);
        fs::create_dir(dir.join("sub")).expect("a directory");
        let start = start(&dir.join("sub")).expect("a start");
        assert_eq!(start.top, dir.canonicalize().expect("a path"));
        assert_eq!(start.head, second);
        // A file whose modification time alone changed counts for nothing.
        fs::File::options()
            .write(true)
            .open(dir.join("a"))
            .and_then(|file| {
                file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            })
            .expect("a file touched");
        assert_eq!(
            summary(dir, &second),
            Ok(Summary {
                commits: 0,
                files: 0
            })
        );

        fs::write(dir.join("ignored"), "x\n").expect("a file");
        fs::write(dir.join("new \"b\""), "b\nno newline").expect("a file");
        fs::write(dir.join("binary"), b"\0\x01").expect("a file");
        assert_eq!(
            summary(dir, &first),
            Ok(Summary {
                commits: 1,
                files: 3
            })
        );
        verify(dir, &first).expect("a commit of the repository");
        let changes = changes(dir, &first).expect("changes");
        assert_eq!(
            changes.patch,
            "diff --git a/a b/a\n\
             index 01e79c3..d8eb098 100644\n\
             --- a/a\n\
             +++ b/a\n\
             @@ -1,3 +1,3 @@\n \
             1\n\
             -2\n\
             +two\n \
             3\n\
             diff --git a/binary b/binary\n\
             new file mode 100644\n\
             Binary files /dev/null and b/binary differ\n\
             diff --git \"a/new \\\"b\\\"\" \"b/new \\\"b\\\"\"\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ \"b/new \\\"b\\\"\"\n\
             @@ -0,0 +1,2 @@\n\
             +b\n\
             +no newline\n\
             \\ No newline at end of file\n"
        );
        let listed: Vec<_> = changes
            .commits
            .iter()
            .map(|commit| {
                (
                    commit.subject.as_str(),
                    commit.included,
                    commit.parent.is_some(),
                )
            })
            .collect();
        assert_eq!(listed, [("second", true, true), ("first", false, false)]);
        assert!(changes.commits.iter().all(|commit| commit.time > 0));
        assert_eq!(old(dir, &first, "a"), Ok(b"1\n2\n3\n".to_vec()));
    }

    #[test]
    fn a_start_must_be_a_full_hash_of_the_repository() {
        let temporary = tempfile::tempdir().expect("a directory");
        let dir = temporary.path();
        let (first, _) = repository(dir);
        assert!(verify(dir, &first[..12]).is_err());
        assert!(verify(dir, "--output=x").is_err());
        assert!(verify(dir, &"0".repeat(40)).is_err());
    }

    #[test]
    fn a_directory_outside_a_working_tree_has_no_start() {
        let temporary = tempfile::tempdir().expect("a directory");
        assert!(start(temporary.path()).is_none());
    }
}

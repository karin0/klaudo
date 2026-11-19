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

/// How many unchanged lines the page shows on each side of a change, and of a line a link
/// names.
pub const CONTEXT: usize = 5;
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

/// How much the tracked files changed since a start, as its link counts it.
#[derive(Debug, PartialEq)]
pub struct Summary {
    pub commits: usize,
    pub files: usize,
    /// The lines added, a binary file counting none.
    pub added: usize,
    pub removed: usize,
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

/// What one commit changed, as a git patch.
#[derive(Serialize)]
pub struct Step {
    pub commit: Commit,
    pub patch: String,
}

/// The latest commits since a start that changed what a page shows, oldest first, and
/// whether more of them came before.
#[derive(Default, Serialize)]
pub struct Steps {
    pub steps: Vec<Step>,
    pub more: bool,
}

/// A file a link names by its path from the top of a working tree, and the lines it
/// names there, from 1, none for the whole file.
pub struct Place {
    pub path: String,
    pub lines: Vec<(usize, usize)>,
}

/// The lines a hunk of a patch without context takes of each side: where they start,
/// or the line before them when there are none, and how many they are.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hunk {
    old: (usize, usize),
    new: (usize, usize),
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

/// The commits, files and lines changed since `since`, which `Stop` counts for its link.
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
    let changed = text(&run(git(top).args([
        "diff-index",
        "--numstat",
        "-z",
        "-M",
        since,
    ]))?);
    let mut summary = Summary {
        commits,
        files: 0,
        added: 0,
        removed: 0,
    };
    // A binary file counts its lines as `-`.
    let lines = |count: &str| match count {
        "-" => Ok(0),
        count => count
            .parse::<usize>()
            .map_err(|error| format!("git diff-index --numstat: {error}")),
    };
    let mut fields = changed.split('\0');
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // A renamed file leaves its path empty, and its two paths follow as fields.
        if path.is_empty() {
            fields.nth(1);
        }
        summary.files += 1;
        summary.added += lines(added)?;
        summary.removed += lines(removed)?;
    }
    Ok(summary)
}

/// The changes since `since`, which `verify` passed, as a git patch, untracked files as
/// new ones.
pub fn patch(top: &Path, since: &str) -> Result<String, String> {
    let mut patch = text(&run(git(top).args([
        "diff-index",
        "--patch",
        "--no-color",
        "--no-ext-diff",
        "-M",
        &format!("--unified={CONTEXT}"),
        "--src-prefix=a/",
        "--dst-prefix=b/",
        since,
    ]))?);
    for path in untracked(top)? {
        added(&mut patch, &path, &top.join(&path));
    }
    Ok(patch)
}

/// The file at `path` as of `since`, `None` when `since` has none there.
pub fn old(top: &Path, since: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
    let object = format!("{since}:{path}");
    if run(git(top).args(["cat-file", "-e", &object])).is_err() {
        return Ok(None);
    }
    run(git(top).args(["show", &object])).map(Some)
}

/// Whether git tracks the file at `path`, or would list it as untracked, since it does
/// not ignore it.
pub fn seen(top: &Path, path: &str) -> Result<bool, String> {
    let listed = run(git(top).args([
        "--literal-pathspecs",
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
        "--",
        path,
    ]))?;
    Ok(!listed.is_empty())
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
pub fn commits(top: &Path, since: &str) -> Result<Vec<Commit>, String> {
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
        let included = included.lines().any(|line| line == hash);
        commits.push(commit(hash, parents, time, subject, included));
    }
    Ok(commits)
}

/// A commit from the fields `git log` prints of it: its hash, its parents' hashes with
/// spaces between, its time in Unix seconds, and its subject.
fn commit(hash: &str, parents: &str, time: &str, subject: &str, included: bool) -> Commit {
    Commit {
        hash: hash.to_owned(),
        parent: parents
            .split(' ')
            .next()
            .filter(|parent| !parent.is_empty())
            .map(str::to_owned),
        time: time.parse().unwrap_or_default(),
        subject: subject.to_owned(),
        included,
    }
}

/// What each commit since `since`, which `verify` passed, changed: all of it, or with
/// `places`, what it changed of their lines, which git follows back from `HEAD` through
/// the commits that moved them.
pub fn steps(top: &Path, since: &str, places: Option<&[Place]>) -> Result<Steps, String> {
    let range = format!("{since}..HEAD");
    // git refuses to follow lines through no commit.
    if run(git(top).args(["rev-list", "--max-count=1", &range]))?.is_empty() {
        return Ok(Steps::default());
    }
    let mut log = git(top);
    log.args([
        "log",
        "--first-parent",
        "--no-color",
        "--no-ext-diff",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--date=unix",
        "--format=%x00%H%x00%P%x00%cd%x00%s%x00",
        &format!("--max-count={}", COMMITS_MAX + 1),
    ]);
    match places {
        None => {
            log.args(["--patch", "--diff-merges=first-parent", "-M"])
                .arg(format!("--unified={CONTEXT}"));
        }
        Some(places) => {
            let followed = followed(top, places)?;
            if followed.is_empty() {
                return Ok(Steps::default());
            }
            log.args(followed);
        }
    }
    let logged = text(&run(log.arg(range))?);
    let mut fields = logged.split('\0').skip(1);
    let mut steps = Vec::new();
    while let (Some(hash), Some(parents), Some(time), Some(subject), Some(patch)) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) {
        steps.push(Step {
            commit: commit(hash, parents, time, subject, true),
            patch: patch.trim_start_matches('\n').to_owned(),
        });
    }
    let more = steps.len() > COMMITS_MAX;
    steps.truncate(COMMITS_MAX);
    steps.reverse();
    Ok(Steps { steps, more })
}

/// The `-L` options that have `git log` follow `places` from `HEAD`. A link names lines
/// of the working tree, so its lines are those of `HEAD` they came from, and a place
/// `HEAD` has no line of is left out.
fn followed(top: &Path, places: &[Place]) -> Result<Vec<String>, String> {
    let mut followed = Vec::new();
    for Place { path, lines } in places {
        let Some(head) = old(top, "HEAD", path)? else {
            continue;
        };
        let count = head.split_inclusive(|byte| *byte == b'\n').count();
        let whole = [(1, count)];
        let hunks = if lines.is_empty() {
            Vec::new()
        } else {
            let patch = text(&run(git(top).args([
                "--literal-pathspecs",
                "diff-index",
                "--patch",
                "--no-color",
                "--no-ext-diff",
                "--unified=0",
                "HEAD",
                "--",
                path,
            ]))?);
            patch.lines().filter_map(hunk).collect()
        };
        for &(from, to) in if lines.is_empty() { &whole[..] } else { lines } {
            if let Some((from, to)) = before(&hunks, from, to)
                && from <= count
            {
                followed.push(format!("-L{from},{}:{path}", to.min(count)));
            }
        }
    }
    Ok(followed)
}

/// The hunk a `line` of a patch opens, as in `@@ -3,2 +4 @@`, where a side of one line
/// leaves its count out.
fn hunk(line: &str) -> Option<Hunk> {
    let mut sides = line.strip_prefix("@@ -")?.split(' ');
    let side = |side: &str| {
        let (start, count) = side.split_once(',').unwrap_or((side, "1"));
        Some((start.parse().ok()?, count.parse().ok()?))
    };
    Some(Hunk {
        old: side(sides.next()?)?,
        new: side(sides.next()?.strip_prefix('+')?)?,
    })
}

/// The lines of the old side that the lines `from` to `to` of the new side came from,
/// through the `hunks` between the sides, in order: a line a hunk added came from the
/// lines the hunk removed. `None` when the old side has none of them.
fn before(hunks: &[Hunk], from: usize, to: usize) -> Option<(usize, usize)> {
    // Where the line `at` came from, as its first line or its last.
    let came = |at: usize, first: bool| {
        let mut shift = 0isize;
        for &Hunk { old, new } in hunks {
            // A hunk adding no line stands after the line it names.
            let (start, end) = (new.0 + usize::from(new.1 == 0), new.0 + new.1);
            if at < start {
                break;
            }
            if at < end {
                // A hunk removing no line names the line before it.
                return if first {
                    old.0 + usize::from(old.1 == 0)
                } else {
                    old.0 + old.1.saturating_sub(1)
                };
            }
            shift += old.1.cast_signed() - new.1.cast_signed();
        }
        at.saturating_add_signed(shift)
    };
    let (from, to) = (came(from, true), came(to, false));
    (from >= 1 && from <= to).then_some((from, to))
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
    fn a_summary_counts_the_lines_of_tracked_files_alone() {
        let temporary = tempfile::tempdir().expect("a directory");
        let dir = temporary.path();
        let (_, second) = repository(dir);
        let git = |args: &[&str]| run(git(dir).args(args)).expect("git runs");
        // Paths holding tabs still read as the paths of a renamed file.
        git(&["mv", "a", "moved\tto\there"]);
        fs::write(dir.join("moved\tto\there"), "1\ntwo\n3\n4\n").expect("an edit");
        fs::write(dir.join("binary"), b"\0\x01").expect("a file");
        git(&["add", "binary"]);
        fs::write(dir.join("untracked"), "u\n").expect("a file");
        assert_eq!(
            summary(dir, &second),
            Ok(Summary {
                commits: 0,
                files: 2,
                added: 1,
                removed: 0
            })
        );
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
                files: 0,
                added: 0,
                removed: 0
            })
        );

        fs::write(dir.join("ignored"), "x\n").expect("a file");
        fs::write(dir.join("new \"b\""), "b\nno newline").expect("a file");
        fs::write(dir.join("binary"), b"\0\x01").expect("a file");
        assert_eq!(
            summary(dir, &first),
            Ok(Summary {
                commits: 1,
                files: 1,
                added: 1,
                removed: 1
            })
        );
        verify(dir, &first).expect("a commit of the repository");
        assert_eq!(
            patch(dir, &first).expect("a patch"),
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
        let commits = commits(dir, &first).expect("commits");
        let listed: Vec<_> = commits
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
        assert!(commits.iter().all(|commit| commit.time > 0));
        assert_eq!(old(dir, &first, "a"), Ok(Some(b"1\n2\n3\n".to_vec())));
        assert_eq!(old(dir, &first, "binary"), Ok(None));
        assert_eq!(seen(dir, "a"), Ok(true));
        assert_eq!(seen(dir, "binary"), Ok(true));
        assert_eq!(seen(dir, "ignored"), Ok(false));
    }

    #[test]
    fn lines_of_the_working_tree_come_from_the_lines_of_head_before_its_changes() {
        // Two lines added after line 2, lines 6 and 7 replaced by one, line 12 removed.
        let patch = "@@ -2,0 +3,2 @@\n+a\n+b\n@@ -6,2 +8 @@\n-x\n-y\n+z\n@@ -12 +12,0 @@\n-w\n";
        let hunks: Vec<Hunk> = patch.lines().filter_map(hunk).collect();
        assert_eq!(
            hunks,
            [((2, 0), (3, 2)), ((6, 2), (8, 1)), ((12, 1), (12, 0))]
                .map(|(old, new)| Hunk { old, new })
        );
        for (new, old) in [
            ((1, 2), Some((1, 2))),
            ((3, 4), None),
            ((2, 5), Some((2, 3))),
            ((4, 6), Some((3, 4))),
            ((8, 8), Some((6, 7))),
            ((7, 9), Some((5, 8))),
            ((12, 13), Some((11, 13))),
            ((13, 13), Some((13, 13))),
        ] {
            assert_eq!(before(&hunks, new.0, new.1), old, "{new:?}");
        }
    }

    #[test]
    fn each_commit_since_a_start_shows_what_it_changed_of_the_lines_followed() {
        let temporary = tempfile::tempdir().expect("a directory");
        let dir = temporary.path();
        let (first, _) = repository(dir);
        let git = |args: &[&str]| {
            run(git(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args))
            .expect("git runs")
        };
        fs::write(dir.join("a"), "0\n1\ntwo\n3\n").expect("an edit");
        git(&["commit", "-qam", "third"]);
        fs::write(dir.join("b"), "b\n").expect("a file");
        git(&["add", "b"]);
        git(&["commit", "-qm", "fourth"]);
        // An uncommitted line above moves the line `two` to line 4.
        fs::write(dir.join("a"), "new\n0\n1\ntwo\n3\n").expect("an edit");

        let subjects = |steps: &Steps| -> Vec<String> {
            let subjects = steps.steps.iter().map(|step| step.commit.subject.clone());
            subjects.collect()
        };
        let all = steps(dir, &first, None).expect("steps");
        assert_eq!(subjects(&all), ["second", "third", "fourth"]);
        assert!(!all.more);
        assert!(all.steps[2].patch.starts_with("diff --git a/b b/b\n"));

        let place = |path: &str, lines: &[(usize, usize)]| Place {
            path: path.to_owned(),
            lines: lines.to_vec(),
        };
        let line = steps(dir, &first, Some(&[place("a", &[(4, 4)])])).expect("steps");
        assert_eq!(subjects(&line), ["second"]);
        assert!(line.steps[0].patch.ends_with("@@ -2,1 +2,1 @@\n-2\n+two\n"));
        let whole = [place("a", &[]), place("untracked", &[(1, 1)])];
        let whole = steps(dir, &first, Some(&whole)).expect("steps");
        assert_eq!(subjects(&whole), ["second", "third"]);
        // A line added since `HEAD` has no commit to show.
        let added = steps(dir, &first, Some(&[place("a", &[(1, 1)])])).expect("steps");
        assert!(added.steps.is_empty());
        let head = start(dir).expect("a start").head;
        let none = steps(dir, &head, Some(&[place("a", &[])])).expect("steps");
        assert!(none.steps.is_empty());
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

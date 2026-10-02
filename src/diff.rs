//! A working tree's unstaged changes, drawn as images: each row a line of the diff on
//! GitHub's dark palette, its text coloured by its syntax and the words a change
//! touched marked inside it. Telegram's markup has no colours, so the colours travel in
//! pictures `pango-view` draws.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use similar::{Algorithm, DiffTag};
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

use crate::process::run;

/// The characters of a line a row holds, which keeps a picture's text legible once a
/// phone fits its width to the screen.
const COLS: usize = 80;
/// Telegram scales a photo down to 2560 pixels on its long side, and a row is 39 pixels
/// tall at the size `draw` uses, so a picture holds this many before it shrinks.
const ROWS_MAX: usize = 60;
/// A file changed in more lines than this is listed without being drawn, which is
/// where lock files and other generated files land.
pub const CHANGED_MAX: usize = 500;
const TAB: usize = 4;

const BACKGROUND: &str = "#0d1117";
const TEXT: Colour = [0xe6, 0xed, 0xf3];
const DIM: &str = "#7d8590";
const HUNK: &str = "#161b33";
const REMOVED: &str = "#3c1618";
const REMOVED_WORD: &str = "#8e1519";
const ADDED: &str = "#12261e";
const ADDED_WORD: &str = "#196c2e";

/// The syntaxes bat ships, which cover TOML and TypeScript among others syntect's own
/// set leaves out.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    two_face::theme::extra()
        .get(EmbeddedThemeName::OneHalfDark)
        .clone()
});

/// One file of the diff.
pub struct Change {
    /// `M`, `A`, `D` or `R`, as `git diff --name-status` writes it.
    pub status: char,
    /// Relative to the top of the working tree.
    pub path: String,
    /// The path a renamed file had.
    pub from: Option<String>,
    pub added: usize,
    pub removed: usize,
    /// The lines of its hunks, absent for a file git takes for binary.
    lines: Option<Vec<String>>,
    /// The top of the working tree.
    root: PathBuf,
}

impl Change {
    pub fn binary(&self) -> bool {
        self.lines.is_none()
    }

    /// The Pango markup of each picture the change is drawn in, empty for a change that
    /// is binary, too large, or a change of mode alone. A hunk can open inside a comment
    /// or a string, so each side is highlighted from the top of its file: the old one as
    /// the index holds it, the new one as the working tree does.
    pub fn pages(&self) -> Result<Vec<String>, String> {
        let Some(lines) = &self.lines else {
            return Ok(Vec::new());
        };
        if lines.is_empty() || self.added + self.removed > CHANGED_MAX {
            return Ok(Vec::new());
        }
        let (old_through, new_through) = reach(lines);
        let old = if self.status == 'A' {
            String::new()
        } else {
            self.indexed()?
        };
        let new = if self.status == 'D' {
            String::new()
        } else {
            self.worktree()?
        };
        let first = new.lines().chain(old.lines()).next().unwrap_or_default();
        let syntax = syntax(&self.path, first);
        let old = highlight(&old, syntax, &THEME, old_through)?;
        let new = highlight(&new, syntax, &THEME, new_through)?;
        Ok(paginate(rows(lines, &old, &new))
            .iter()
            .map(|page| markup(page))
            .collect())
    }

    fn worktree(&self) -> Result<String, String> {
        let file = self.root.join(&self.path);
        let bytes = std::fs::read(&file).map_err(|error| format!("{}: {error}", file.display()))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The file as the index holds it, which is what an unstaged change starts from.
    fn indexed(&self) -> Result<String, String> {
        let path = self.from.as_ref().unwrap_or(&self.path);
        let mut git = Command::new("git");
        git.arg("-C")
            .arg(&self.root)
            .arg("show")
            .arg(format!(":{path}"));
        Ok(String::from_utf8_lossy(&run(&mut git)?).into_owned())
    }
}

/// The unstaged changes of the working tree holding `dir`, file by file. The prefixes
/// and quoting are spelled out, so a user's git configuration leaves the output as this
/// parser reads it.
pub fn changes(dir: &Path) -> Result<Vec<Change>, String> {
    let mut top = Command::new("git");
    top.arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"]);
    let root = PathBuf::from(String::from_utf8_lossy(&run(&mut top)?).trim_end());
    let mut git = Command::new("git");
    git.arg("-C").arg(&root).args([
        "-c",
        "core.quotePath=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "-M",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ]);
    let output = run(&mut git)?;
    Ok(parse(&String::from_utf8_lossy(&output), &root))
}

/// Draws `markup` into the PNG at `png`.
pub fn draw(markup: &str, png: &Path) -> Result<(), String> {
    let source = png.with_extension("pango");
    std::fs::write(&source, markup).map_err(|error| format!("{}: {error}", source.display()))?;
    run(Command::new("pango-view")
        .args([
            "--markup",
            "-q",
            "--font=Noto Sans Mono,monospace 10",
            "--dpi=200",
            "--margin=20",
            &format!("--background={BACKGROUND}"),
            &format!("--foreground={}", hex(TEXT)),
            "-o",
        ])
        .arg(png)
        .arg(&source))
    .map(drop)
}

fn parse(diff: &str, root: &Path) -> Vec<Change> {
    let mut changes: Vec<Change> = Vec::new();
    let mut in_hunks = false;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            // `a/<path> b/<path>`, the two paths equal for anything but a rename, which
            // names its own.
            let length = header.len().saturating_sub(5) / 2;
            changes.push(Change {
                status: 'M',
                path: header.get(2..2 + length).unwrap_or(header).to_owned(),
                from: None,
                added: 0,
                removed: 0,
                lines: Some(Vec::new()),
                root: root.to_owned(),
            });
            in_hunks = false;
            continue;
        }
        let Some(change) = changes.last_mut() else {
            continue;
        };
        if in_hunks || line.starts_with("@@") {
            in_hunks = true;
            match line.as_bytes().first() {
                Some(b'+') => change.added += 1,
                Some(b'-') => change.removed += 1,
                _ => {}
            }
            if let Some(lines) = &mut change.lines {
                lines.push(line.to_owned());
            }
        } else if line.starts_with("new file mode") {
            change.status = 'A';
        } else if line.starts_with("deleted file mode") {
            change.status = 'D';
        } else if let Some(from) = line.strip_prefix("rename from ") {
            change.from = Some(from.to_owned());
        } else if let Some(to) = line.strip_prefix("rename to ") {
            change.status = 'R';
            to.clone_into(&mut change.path);
        } else if line.starts_with("Binary files ") {
            change.lines = None;
        }
    }
    changes
}

/// Where a hunk starts on each side, from `@@ -<old>[,n] +<new>[,n] @@`, with how many
/// lines it spans there.
fn ranges(header: &str) -> Option<(Range<usize>, Range<usize>)> {
    let mut fields = header.split(' ').skip(1);
    let side = |field: Option<&str>, sign: char| -> Option<Range<usize>> {
        let (start, count) = match field?.strip_prefix(sign)?.split_once(',') {
            Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
            None => (field?.get(1..)?.parse().ok()?, 1),
        };
        Some(start..start + count)
    };
    Some((side(fields.next(), '-')?, side(fields.next(), '+')?))
}

/// The last line of each side any hunk shows, which is as far as highlighting has to go.
fn reach(lines: &[String]) -> (usize, usize) {
    lines
        .iter()
        .filter_map(|line| ranges(line))
        .fold((0, 0), |(old, new), (old_range, new_range)| {
            (old.max(old_range.end), new.max(new_range.end))
        })
}

/// What a file is written in, by its extension or its name, which is how bat's syntaxes
/// list a `Makefile`, or else by its first line, which names a script's interpreter.
fn syntax(path: &str, first: &str) -> &'static SyntaxReference {
    let path = Path::new(path);
    let lookup = |key: Option<&std::ffi::OsStr>| SYNTAXES.find_syntax_by_extension(key?.to_str()?);
    lookup(path.extension())
        .or_else(|| lookup(path.file_name()))
        .or_else(|| SYNTAXES.find_syntax_by_first_line(first))
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text())
}

type Colour = [u8; 3];

/// A character of a row, its colour, and whether a change touched it.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Cell {
    character: char,
    colour: Colour,
    changed: bool,
}

const NEWLINE: Cell = Cell {
    character: '\n',
    colour: TEXT,
    changed: false,
};

/// The first `through` lines of `text`, each as cells coloured by `syntax` in `theme`,
/// tabs expanded.
fn highlight(
    text: &str,
    syntax: &SyntaxReference,
    theme: &Theme,
    through: usize,
) -> Result<Vec<Vec<Cell>>, String> {
    let mut highlighter = HighlightLines::new(syntax, theme);
    LinesWithEndings::from(text)
        .take(through)
        .map(|line| {
            let line = format!("{}\n", expand(line.trim_end_matches(['\n', '\r'])));
            let styled = highlighter
                .highlight_line(&line, &SYNTAXES)
                .map_err(|error| error.to_string())?;
            Ok(styled
                .into_iter()
                .flat_map(|(style, piece)| {
                    let colour = [style.foreground.r, style.foreground.g, style.foreground.b];
                    piece
                        .chars()
                        .filter(|&character| character != '\n')
                        .map(move |character| Cell {
                            character,
                            colour,
                            changed: false,
                        })
                })
                .collect())
        })
        .collect()
}

fn plain(text: &str) -> Vec<Cell> {
    text.chars()
        .map(|character| Cell {
            character,
            colour: TEXT,
            changed: false,
        })
        .collect()
}

/// A line of the diff as cells: the highlighted line `number` of its side, or for a file
/// that changed again since git read it, its text uncoloured.
fn line_cells(line: &str, side: &[Vec<Cell>], number: usize) -> Vec<Cell> {
    let text = expand(line.get(1..).unwrap_or_default().trim_end_matches('\r'));
    match number.checked_sub(1).and_then(|index| side.get(index)) {
        Some(cells) if cells.iter().map(|cell| cell.character).eq(text.chars()) => cells.clone(),
        _ => plain(&text),
    }
}

enum Row {
    Hunk(String),
    Context(Vec<Cell>),
    Removed(Vec<Cell>),
    Added(Vec<Cell>),
    /// What git says about a line, such as that a file ends without a newline.
    Note(String),
}

/// The rows the lines of a file's hunks fill, hunk by hunk, coloured from the
/// highlighted sides `old` and `new`. A run of removed lines and the added lines after
/// it are compared as two texts, so a statement split over more lines or joined onto
/// fewer still marks only the words that changed.
fn rows(lines: &[String], old: &[Vec<Cell>], new: &[Vec<Cell>]) -> Vec<Vec<Row>> {
    let mut hunks: Vec<Vec<Row>> = Vec::new();
    let (mut old_line, mut new_line) = (0, 0);
    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];
        if let Some((old_range, new_range)) = ranges(line) {
            (old_line, new_line) = (old_range.start, new_range.start);
            hunks.push(vec![Row::Hunk(line.chars().take(COLS + 2).collect())]);
            index += 1;
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            index += 1;
            continue;
        };
        match line.as_bytes().first() {
            Some(b'-' | b'+') => {
                let removed = run_of(&lines[index..], '-');
                let added = run_of(&lines[index + removed..], '+');
                let side = |lines: &[String], cells: &[Vec<Cell>], first: usize| {
                    let rows: Vec<Vec<Cell>> = lines
                        .iter()
                        .zip(first..)
                        .map(|(line, number)| line_cells(line, cells, number))
                        .collect();
                    rows.join(&NEWLINE)
                };
                let mut removed_cells = side(&lines[index..index + removed], old, old_line);
                let mut added_cells = side(
                    &lines[index + removed..index + removed + added],
                    new,
                    new_line,
                );
                mark(&mut removed_cells, &mut added_cells);
                // An empty line is a row of its own, while a side of no lines has none.
                if removed > 0 {
                    hunk.extend(wrap(&removed_cells).into_iter().map(Row::Removed));
                }
                if added > 0 {
                    hunk.extend(wrap(&added_cells).into_iter().map(Row::Added));
                }
                old_line += removed;
                new_line += added;
                index += removed + added;
            }
            Some(b'\\') => {
                hunk.push(Row::Note(line.clone()));
                index += 1;
            }
            _ => {
                let cells = line_cells(line, new, new_line);
                hunk.extend(wrap(&cells).into_iter().map(Row::Context));
                old_line += 1;
                new_line += 1;
                index += 1;
            }
        }
    }
    hunks
}

fn run_of(lines: &[String], sign: char) -> usize {
    lines
        .iter()
        .take_while(|line| line.starts_with(sign))
        .count()
}

fn expand(line: &str) -> String {
    let mut expanded = String::with_capacity(line.len());
    for character in line.chars() {
        if character == '\t' {
            let width = TAB - expanded.chars().count() % TAB;
            expanded.extend(std::iter::repeat_n(' ', width));
        } else {
            expanded.push(character);
        }
    }
    expanded
}

/// Marks the cells of both texts where their words differ. A side with nothing to
/// compare against is a plain addition or removal, so nothing in it is marked.
fn mark(old: &mut [Cell], new: &mut [Cell]) {
    if old.is_empty() || new.is_empty() {
        return;
    }
    let old_text: Vec<char> = old.iter().map(|cell| cell.character).collect();
    let new_text: Vec<char> = new.iter().map(|cell| cell.character).collect();
    let (old_words, new_words) = (words(&old_text), words(&new_text));
    let slices = |text: &[char], words: &[Range<usize>]| -> Vec<Vec<char>> {
        words
            .iter()
            .map(|word| text[word.clone()].to_vec())
            .collect()
    };
    let ops = similar::capture_diff_slices(
        Algorithm::Myers,
        &slices(&old_text, &old_words),
        &slices(&new_text, &new_words),
    );
    for op in ops {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        for word in &old_words[old_range] {
            old[word.clone()]
                .iter_mut()
                .for_each(|cell| cell.changed = true);
        }
        for word in &new_words[new_range] {
            new[word.clone()]
                .iter_mut()
                .for_each(|cell| cell.changed = true);
        }
    }
}

/// Where the words of a text are, each a run of letters, digits and underscores, or any
/// other single character.
fn words(text: &[char]) -> Vec<Range<usize>> {
    let wordy = |character: char| character.is_alphanumeric() || character == '_';
    let mut cut = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = if wordy(text[start]) {
            start
                + text[start..]
                    .iter()
                    .take_while(|&&character| wordy(character))
                    .count()
        } else {
            start + 1
        };
        cut.push(start..end);
        start = end;
    }
    cut
}

/// Lines cut at newlines and wherever they run past `COLS`.
fn wrap(cells: &[Cell]) -> Vec<Vec<Cell>> {
    let mut rows = vec![Vec::new()];
    for &cell in cells {
        let row = rows.last_mut().expect("a row");
        if cell.character == '\n' {
            rows.push(Vec::new());
        } else if row.len() == COLS {
            rows.push(vec![cell]);
        } else {
            row.push(cell);
        }
    }
    rows
}

/// The hunks laid out over pictures of at most `ROWS_MAX` rows, a hunk starting a new
/// picture where it would not fit the rest of the current one.
fn paginate(hunks: Vec<Vec<Row>>) -> Vec<Vec<Row>> {
    let mut pages: Vec<Vec<Row>> = Vec::new();
    for hunk in hunks {
        if pages
            .last()
            .is_none_or(|page| page.len() + hunk.len() > ROWS_MAX)
        {
            pages.push(Vec::new());
        }
        for row in hunk {
            if pages.last().is_some_and(|page| page.len() == ROWS_MAX) {
                pages.push(Vec::new());
            }
            pages.last_mut().expect("a page").push(row);
        }
    }
    pages
}

fn markup(page: &[Row]) -> String {
    let rows: Vec<String> = page
        .iter()
        .map(|row| match row {
            Row::Hunk(text) => span(&pad(text, COLS + 2), Some(HUNK), Some(DIM)),
            Row::Context(cells) => painted(' ', cells, None, None),
            Row::Note(text) => span(text, None, Some(DIM)),
            Row::Removed(cells) => painted('-', cells, Some(REMOVED), Some(REMOVED_WORD)),
            Row::Added(cells) => painted('+', cells, Some(ADDED), Some(ADDED_WORD)),
        })
        .collect();
    rows.join("\n")
}

/// A row of a line, filled to the picture's width so a changed line's colour spans it.
fn painted(sign: char, cells: &[Cell], line: Option<&str>, word: Option<&str>) -> String {
    let padding = std::iter::repeat_n(
        Cell {
            character: ' ',
            colour: TEXT,
            changed: false,
        },
        COLS.saturating_sub(cells.len()),
    );
    let cells: Vec<Cell> = cells.iter().copied().chain(padding).collect();
    let mut out = format!("{sign} ");
    for run in cells.chunk_by(|a, b| (a.colour, a.changed) == (b.colour, b.changed)) {
        let text: String = run.iter().map(|cell| cell.character).collect();
        let background = word.filter(|_| run[0].changed);
        out.push_str(&span(&text, background, Some(&hex(run[0].colour))));
    }
    match line {
        Some(line) => format!("<span background=\"{line}\">{out}</span>"),
        None => out,
    }
}

fn hex([red, green, blue]: Colour) -> String {
    format!("#{red:02x}{green:02x}{blue:02x}")
}

fn pad(text: &str, width: usize) -> String {
    let length = text.chars().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(length)))
}

fn span(text: &str, background: Option<&str>, foreground: Option<&str>) -> String {
    let background = background.map(|colour| format!(" background=\"{colour}\""));
    let foreground = foreground.map(|colour| format!(" foreground=\"{colour}\""));
    format!(
        "<span{}{}>{}</span>",
        background.unwrap_or_default(),
        foreground.unwrap_or_default(),
        escape(text)
    )
}

/// The characters Pango's markup reserves.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/a b.rs b/src/a b.rs
index 1..2 100644
--- a/src/a b.rs
+++ b/src/a b.rs
@@ -1,3 +1,3 @@ fn main() {
 keep
-let x = compose(a,
-    b);
+let x = compose(a, c);
\\ No newline at end of file
diff --git a/old.txt b/new.txt
similarity index 90%
rename from old.txt
rename to new.txt
@@ -1 +1 @@
-one
+two
diff --git a/gone b/gone
deleted file mode 100644
index 1..0
--- a/gone
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/logo.png b/logo.png
index 1..2 100644
Binary files a/logo.png and b/logo.png differ
";

    fn marks(cells: &[Cell]) -> String {
        cells
            .iter()
            .map(|cell| if cell.changed { '^' } else { cell.character })
            .collect()
    }

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|&line| line.to_owned()).collect()
    }

    #[test]
    fn a_diff_is_read_file_by_file() {
        let changes = parse(DIFF, Path::new("/tree"));
        let read: Vec<_> = changes
            .iter()
            .map(|change| {
                (
                    change.status,
                    change.from.as_deref(),
                    change.path.as_str(),
                    change.added,
                    change.removed,
                    change.binary(),
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                ('M', None, "src/a b.rs", 1, 2, false),
                ('R', Some("old.txt"), "new.txt", 1, 1, false),
                ('D', None, "gone", 0, 1, false),
                ('M', None, "logo.png", 0, 0, true),
            ]
        );
        assert_eq!(changes[3].pages(), Ok(Vec::new()));
    }

    #[test]
    fn a_hunk_header_names_where_each_side_starts() {
        assert_eq!(ranges("@@ -3,4 +5 @@ fn main"), Some((3..7, 5..6)));
        assert_eq!(ranges("@@ -1 +0,0 @@"), Some((1..2, 0..0)));
        assert_eq!(ranges(" keep"), None);
        assert_eq!(
            reach(&owned(&["@@ -3,4 +5 @@", "@@ -9,2 +2,1 @@"])),
            (11, 6)
        );
    }

    #[test]
    fn a_reflowed_statement_marks_only_the_words_that_changed() {
        let mut old = [plain("let x = compose(a,"), plain("    b);")].join(&NEWLINE);
        let mut new = plain("let x = compose(a, c);");
        mark(&mut old, &mut new);
        assert_eq!(marks(&old), "let x = compose(a,^^ ^^^);");
        assert_eq!(marks(&new), "let x = compose(a, ^);");
        let mut added = plain("new");
        mark(&mut added, &mut []);
        assert_eq!(marks(&added), "new");
    }

    #[test]
    fn each_side_is_coloured_from_its_own_file() {
        let rust = syntax("src/main.rs", "");
        let old = highlight("fn a() {}\n", rust, &THEME, 1).expect("highlighted");
        let new = highlight("// b\nfn b() {}\n", rust, &THEME, 2).expect("highlighted");
        let hunks = rows(
            &owned(&["@@ -1 +1,2 @@", "-fn a() {}", "+// b", "+fn b() {}"]),
            &old,
            &new,
        );
        let [
            Row::Hunk(_),
            Row::Removed(removed),
            Row::Added(comment),
            Row::Added(added),
        ] = &hunks[0][..]
        else {
            panic!("a hunk of a removal and two additions");
        };
        let colours = |cells: &[Cell]| cells.iter().map(|cell| cell.colour).collect::<Vec<_>>();
        assert_eq!(colours(removed), colours(&old[0]));
        assert_eq!(colours(comment), colours(&new[0]));
        assert_eq!(colours(added), colours(&new[1]));
        assert_ne!(removed[0].colour, removed[3].colour, "a keyword stands out");
        assert_ne!(comment[0].colour, added[0].colour, "a comment reads as one");
        let added_only = rows(&owned(&["@@ -0,0 +1 @@", "+// b"]), &[], &new);
        assert!(matches!(&added_only[0][..], [Row::Hunk(_), Row::Added(_)]));
        // A line the file no longer holds as git showed it is left uncoloured.
        let stale = line_cells("+fn c() {}", &new, 2);
        assert!(stale.iter().all(|cell| cell.colour == TEXT));
    }

    #[test]
    fn a_long_line_wraps_and_a_tab_reaches_the_next_stop() {
        assert_eq!(expand("a\tb\t\tc"), "a   b       c");
        let line = plain(&"x".repeat(COLS + 1));
        let rows = wrap(&line);
        assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), [COLS, 1]);
    }

    #[test]
    fn a_hunk_that_overflows_a_picture_starts_the_next() {
        let hunk =
            |rows: usize| -> Vec<Row> { (0..rows).map(|_| Row::Context(Vec::new())).collect() };
        let lengths = |hunks| -> Vec<usize> { paginate(hunks).iter().map(Vec::len).collect() };
        assert_eq!(lengths(vec![hunk(30), hunk(20)]), [50]);
        assert_eq!(lengths(vec![hunk(30), hunk(40)]), [30, 40]);
        assert_eq!(
            lengths(vec![hunk(10), hunk(ROWS_MAX + 5)]),
            [10, ROWS_MAX, 5]
        );
    }

    #[test]
    fn a_changed_row_spans_the_picture_in_its_colour() {
        let mut cells = plain("a<");
        cells[1].changed = true;
        let row = painted('+', &cells, Some(ADDED), Some(ADDED_WORD));
        let text = hex(TEXT);
        assert_eq!(
            row,
            format!(
                "<span background=\"{ADDED}\">+ \
                 <span foreground=\"{text}\">a</span>\
                 <span background=\"{ADDED_WORD}\" foreground=\"{text}\">&lt;</span>\
                 <span foreground=\"{text}\">{}</span></span>",
                " ".repeat(COLS - 2)
            )
        );
    }
}

import { useMemo, useRef, useState } from 'react';
import { parsePatchFiles } from '@pierre/diffs';
import {
  CodeView,
  useStableCallback,
  type CodeViewDiffItem,
  type CodeViewHandle,
  type CodeViewReactOptions,
  type DiffLineAnnotation,
  type FileDiffMetadata,
  type SelectedLineRange,
  type SelectionSide,
} from '@pierre/diffs/react';
import {
  app,
  call,
  failure,
  type Commit,
  type Opened,
  type Range,
} from './api';
import {
  EMPTY,
  Editor,
  NoteView,
  Overview,
  Sending,
  Sheet,
  THEMES,
  keepFocus,
  span,
  useKeyboardReveal,
  useScheme,
  version,
  type Draft,
} from './notes';

type Changes = Extract<Opened, { kind: 'changes' }>;

/** A line as the library's selection names it. */
interface Point {
  side: SelectionSide;
  line: number;
}

/**
 * A row of a file's diff as the unified view shows it. A removed row is on the
 * deletions side, numbered as of the start. Any other row is on the additions side,
 * numbered as the file is, and `old` numbers a context row as of the start.
 */
interface Row extends Point {
  old: number;
  mark: ' ' | '-' | '+';
  text: string;
}

/** The rows of a file from `start` to `end`, in the order the view shows them. */
interface Span {
  path: string;
  start: Point;
  end: Point;
}

/** What a span covers: the lines it spans on each side, and the rows as a diff shows them. */
interface Quote {
  new: Range | null;
  old: Range | null;
  lines: string[];
  /** The lines a suggestion rewrites, `null` when the span holds removed ones. */
  original: string | null;
}

interface Note {
  span: Span;
  quote: Quote;
  draft: Draft;
}

/** What a line carries under it: a saved note, or the span the editor is open on. */
type Extension = Note | Span;

/** A line of the library's arrays without the newline they keep on it. */
const unended = (line: string | undefined) => line?.replace(/\n$/, '') ?? '';

/**
 * The rows of `diff`: those of its hunks, and once the library has loaded the whole of
 * both sides, the unchanged lines between them, which expanding a hunk shows.
 */
function rows(diff: FileDiffMetadata): Row[] {
  const all: Row[] = [];
  let [oldAt, newAt] = [0, 0];
  const between = (oldTo: number, newTo: number) => {
    if (diff.isPartial) {
      [oldAt, newAt] = [oldTo, newTo];
      return;
    }
    for (; newAt < newTo; oldAt++, newAt++) {
      const text = unended(diff.additionLines[newAt]);
      all.push({
        side: 'additions',
        line: newAt + 1,
        old: oldAt + 1,
        mark: ' ',
        text,
      });
    }
  };
  for (const hunk of diff.hunks) {
    // The arrays hold only the hunks' lines until the library loads the whole file.
    const oldBase = hunk.deletionStart - hunk.deletionLineIndex;
    const newBase = hunk.additionStart - hunk.additionLineIndex;
    for (const block of hunk.hunkContent) {
      between(block.deletionLineIndex, block.additionLineIndex);
      const removed = block.type === 'context' ? 0 : block.deletions;
      const added = block.type === 'context' ? block.lines : block.additions;
      for (let index = 0; index < removed; index++) {
        const at = block.deletionLineIndex + index;
        const text = unended(diff.deletionLines[at]);
        all.push({
          side: 'deletions',
          line: at + oldBase,
          old: at + oldBase,
          mark: '-',
          text,
        });
      }
      for (let index = 0; index < added; index++) {
        const at = block.additionLineIndex + index;
        all.push({
          side: 'additions',
          line: at + newBase,
          old: block.deletionLineIndex + index + oldBase,
          mark: block.type === 'context' ? ' ' : '+',
          text: unended(diff.additionLines[at]),
        });
      }
      oldAt =
        block.deletionLineIndex + (block.type === 'context' ? added : removed);
      newAt = block.additionLineIndex + added;
    }
  }
  between(diff.deletionLines.length, diff.additionLines.length);
  return all;
}

/** Where `point` is among `rows`: a context row answers to its number on either side. */
function find(rows: readonly Row[], { side, line }: Point): number {
  return rows.findIndex((row) =>
    side === 'deletions'
      ? row.mark === '-'
        ? row.line === line
        : row.mark === ' ' && row.old === line
      : row.mark !== '-' && row.line === line,
  );
}

const point = ({ side, line }: Row): Point => ({ side, line });

/** The span of `diff` between `one` and `other`, whichever the view shows first. */
function spanned(diff: FileDiffMetadata, one: Point, other: Point): Span {
  const all = rows(diff);
  const ends = [find(all, one), find(all, other)];
  const [start, end] = [all[Math.min(...ends)], all[Math.max(...ends)]];
  return start === undefined || end === undefined
    ? { path: diff.name, start: one, end: other }
    : { path: diff.name, start: point(start), end: point(end) };
}

function quoted(diff: FileDiffMetadata, { start, end }: Span): Quote {
  const all = rows(diff);
  const shown = all.slice(find(all, start), find(all, end) + 1);
  const reach = (side: SelectionSide): Range | null => {
    const lines = shown
      .filter((row) => row.side === side)
      .map((row) => row.line);
    return lines.length === 0
      ? null
      : { from: Math.min(...lines), to: Math.max(...lines) };
  };
  const old = reach('deletions');
  return {
    new: reach('additions'),
    old,
    lines: shown.map((row) => row.mark + row.text),
    original: old === null ? shown.map((row) => row.text).join('\n') : null,
  };
}

function label(quote: Quote): string {
  const { new: added, old } = quote;
  if (old === null) {
    return added === null ? '' : span(added);
  }
  const removed =
    old.from === old.to
      ? `line ${String(old.from)}`
      : `lines ${String(old.from)}–${String(old.to)}`;
  return added === null
    ? `Removed ${removed}`
    : `${span(added)}, replacing ${removed}`;
}

const key = ({ path, end }: Span) =>
  `${path}\n${end.side}\n${String(end.line)}`;

function range({ start, end }: Span): SelectedLineRange {
  return {
    start: start.line,
    side: start.side,
    end: end.line,
    endSide: end.side,
  };
}

/** The start of the changes, which a tap opens a sheet to move. */
function Commits({
  changes,
  onMove,
}: {
  changes: Changes;
  onMove: (since: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [head] = changes.commits;
  const oldest = changes.commits.find(
    (commit) => commit.parent === changes.since,
  );
  const shown =
    changes.since === head?.hash
      ? 'Working tree only'
      : (oldest?.subject ?? changes.since.slice(0, 7));
  return (
    <section className="section">
      <button
        className="start"
        onClick={() => {
          setOpen(true);
        }}
      >
        <span>Changes from</span>
        <span className="hint value">{shown}</span>
        <span className="hint">›</span>
      </button>
      {open && (
        <Starts
          changes={changes}
          onClose={() => {
            setOpen(false);
          }}
          onMove={onMove}
        />
      )}
    </section>
  );
}

/**
 * A sheet of starts: `HEAD`, which leaves the working tree alone, then the parent of
 * each commit the changes take in, newest first, then of each commit before the start.
 */
function Starts({
  changes,
  onClose,
  onMove,
}: {
  changes: Changes;
  onClose: () => void;
  onMove: (since: string) => void;
}) {
  const date = (time: number) =>
    new Date(time * 1000).toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    });
  const cell = (
    key: string,
    start: string | null,
    title: string,
    hint = '',
  ) => (
    <button
      key={key}
      className="cell"
      disabled={start === null}
      onClick={() => {
        onClose();
        if (start !== null && start !== changes.since) {
          app.HapticFeedback.selectionChanged();
          onMove(start);
        }
      }}
    >
      <span className="grow">
        <span className="title">{title}</span>
        {hint && <span className="hint">{hint}</span>}
      </span>
      {start === changes.since && <span className="check">✓</span>}
    </button>
  );
  const commit = (commit: Commit) =>
    cell(
      commit.hash,
      commit.parent,
      commit.subject,
      `${commit.hash.slice(0, 7)} · ${date(commit.time)}`,
    );
  const group = (label: string, commits: Commit[]) =>
    commits.length > 0 && (
      <>
        <div className="label">{label}</div>
        <div className="group">{commits.map(commit)}</div>
      </>
    );
  const [head] = changes.commits;
  return (
    <Sheet heading="Changes from" onClose={onClose}>
      <div className="starts">
        {head !== undefined && (
          <div className="group">
            {cell('head', head.hash, 'Working tree only')}
          </div>
        )}
        {group(
          'Included',
          changes.commits.filter((commit) => commit.included),
        )}
        {group(
          'Earlier',
          changes.commits.filter((commit) => !commit.included),
        )}
      </div>
    </Sheet>
  );
}

/** How many lines `fileDiff` removes and adds, as its header counts them. */
function counts(fileDiff: FileDiffMetadata): string {
  const sum = (lines: 'deletionLines' | 'additionLines') =>
    fileDiff.hunks.reduce((total, hunk) => total + hunk[lines], 0);
  const [removed, added] = [sum('deletionLines'), sum('additionLines')];
  return [
    removed > 0 && `-${String(removed)}`,
    added > 0 && `+${String(added)}`,
  ]
    .filter(Boolean)
    .join(' ');
}

/** The files the changes touch, each with a button that scrolls to it. */
function Files({
  files,
  onJump,
}: {
  files: readonly FileDiffMetadata[];
  onJump: (path: string) => void;
}) {
  return (
    <section className="section">
      <div className="label">Files</div>
      {files.map((fileDiff) => (
        <button
          key={fileDiff.name}
          className="file"
          onClick={() => {
            onJump(fileDiff.name);
          }}
        >
          <span className="path">
            {fileDiff.prevName === undefined
              ? fileDiff.name
              : `${fileDiff.prevName} → ${fileDiff.name}`}
          </span>
          <span className="hint">{counts(fileDiff)}</span>
        </button>
      ))}
    </section>
  );
}

export function ChangesReview({
  changes,
  onMove,
}: {
  changes: Changes;
  onMove: (since: string) => void;
}) {
  const scheme = useScheme();
  useKeyboardReveal();
  const [notes, setNotes] = useState<ReadonlyMap<string, Note>>(new Map());
  const [editing, setEditing] = useState<Span | null>(null);
  const [dragged, setDragged] = useState<Span | null>(null);
  const dragging = useRef(false);
  const [draft, setDraft] = useState<Draft>(EMPTY);
  const [folded, setFolded] = useState<ReadonlySet<string>>(new Set());
  const view = useRef<CodeViewHandle<Extension, undefined>>(null);

  // The library loads a file whole into its metadata in place, so each stays one object.
  const files = useMemo(
    () => parsePatchFiles(changes.patch).flatMap((patch) => patch.files),
    [changes],
  );
  const diffs = useMemo(
    () => new Map(files.map((fileDiff) => [fileDiff.name, fileDiff])),
    [files],
  );
  const diff = (path: string) => {
    const found = diffs.get(path);
    if (found === undefined) {
      throw new Error(`${path} is in no diff`);
    }
    return found;
  };

  const review = () => ({
    since: changes.since,
    notes: [...notes.values()].map(({ span, quote, draft }) => {
      const fileDiff = diff(span.path);
      const old = fileDiff.prevName ?? fileDiff.name;
      return {
        new: quote.new && { path: fileDiff.name, ...quote.new },
        old: quote.old && { path: old, ...quote.old },
        lines: quote.lines,
        text: draft.text.trim(),
        suggestion: draft.suggestion,
      };
    }),
  });

  const moved = (since: string) => {
    if (notes.size === 0 && editing === null) {
      onMove(since);
      return;
    }
    app.showConfirm('Moving the start drops your comments.', (confirmed) => {
      if (confirmed) {
        onMove(since);
      }
    });
  };

  // A tap on a line number opens the editor there, and a tap on another in the same file
  // while it is open stretches it over the lines between.
  const tapped = (path: string, at: Point) => {
    const fileDiff = diff(path);
    if (editing === null) {
      const here = spanned(fileDiff, at, at);
      const note = notes.get(key(here));
      setEditing(note?.span ?? here);
      setDraft(note?.draft ?? EMPTY);
    } else if (editing.path === path) {
      setEditing(spanned(fileDiff, editing.start, at));
    }
  };

  const started = useStableCallback(() => {
    dragging.current = false;
    setDragged(null);
  });
  const across = (path: string, range: SelectedLineRange): Span =>
    spanned(
      diff(path),
      { side: range.side ?? 'additions', line: range.start },
      { side: range.endSide ?? range.side ?? 'additions', line: range.end },
    );
  const changed = useStableCallback(
    (range: SelectedLineRange | null, context: { item: { id: string } }) => {
      dragging.current = true;
      setDragged(range === null ? null : across(context.item.id, range));
    },
  );
  // The library reports a tap on the one line selected as `null`, which leaves the
  // editor open, since only its Cancel button drops the draft.
  const selected = useStableCallback(
    (range: SelectedLineRange | null, context: { item: { id: string } }) => {
      const drag = dragging.current;
      started();
      if (range === null) {
        return;
      }
      const path = context.item.id;
      if (!drag) {
        tapped(path, { side: range.side ?? 'additions', line: range.start });
        return;
      }
      if (editing === null) {
        setDraft(EMPTY);
      }
      setEditing(across(path, range));
    },
  );

  const loaded = useStableCallback(async (fileDiff: FileDiffMetadata) => {
    const prev = fileDiff.prevName ?? fileDiff.name;
    const query = new URLSearchParams({
      since: changes.since,
      path: fileDiff.name,
      prev,
    });
    try {
      const response = await call(`api/contents?${query.toString()}`);
      const contents = (await response.json()) as { old: string; new: string };
      const newFile = { name: fileDiff.name, contents: contents.new };
      // The library takes no old side for a file renamed and left as it was.
      return fileDiff.type === 'rename-pure'
        ? { oldFile: null, newFile }
        : { oldFile: { name: prev, contents: contents.old }, newFile };
    } catch (error) {
      app.showAlert(failure(error));
      throw error;
    }
  });

  const options = useMemo(
    (): CodeViewReactOptions<Extension, undefined> => ({
      theme: THEMES,
      themeType: scheme,
      diffStyle: 'unified',
      overflow: 'wrap',
      stickyHeaders: true,
      enableLineSelection: true,
      loadDiffFiles: loaded,
      onLineSelectionStart: started,
      onLineSelectionChange: changed,
      onLineSelectionEnd: selected,
      unsafeCSS:
        ':host { --diffs-light-bg: var(--bg); --diffs-dark-bg: var(--bg); }',
    }),
    [scheme, loaded, started, changed, selected],
  );

  const items = useMemo(
    () =>
      files.map((fileDiff): CodeViewDiffItem<Extension> => {
        const path = fileDiff.name;
        const saved = [...notes.values()].filter(
          (note) =>
            note.span.path === path &&
            (editing === null || key(note.span) !== key(editing)),
        );
        const at = ({ side, line }: Point) => ({ side, lineNumber: line });
        const annotations: DiffLineAnnotation<Extension>[] = [
          ...(editing?.path === path
            ? [{ ...at(editing.end), metadata: editing }]
            : []),
          ...saved.map((note) => ({ ...at(note.span.end), metadata: note })),
        ];
        const collapsed = folded.has(path);
        return {
          id: path,
          type: 'diff',
          fileDiff,
          annotations,
          collapsed,
          version: version(fileDiff, annotations, collapsed),
        };
      }),
    [files, notes, editing, folded],
  );

  const fold = (path: string) => {
    const next = new Set(folded);
    if (!next.delete(path)) {
      next.add(path);
    }
    setFolded(next);
  };
  const jump = (path: string) => {
    view.current?.scrollTo({ type: 'item', id: path, align: 'start' });
  };

  const order = (note: Note): [number, number] => {
    const fileDiff = diff(note.span.path);
    return [files.indexOf(fileDiff), find(rows(fileDiff), note.span.end)];
  };
  const entries = [...notes.values()]
    .map((note) => ({ note, at: order(note) }))
    .sort(
      ({ at: [file, row] }, { at: [otherFile, otherRow] }) =>
        file - otherFile || row - otherRow,
    )
    .map(({ note: { span, quote, draft } }) => ({
      key: key(span),
      label: `${span.path} · ${label(quote)}`,
      draft,
      onJump: () => {
        const scroll = () => {
          view.current?.scrollTo({
            type: 'line',
            id: span.path,
            lineNumber: span.end.line,
            side: span.end.side,
            align: 'center',
          });
        };
        // The lines of a folded file have no place to scroll to until it unfolds.
        if (folded.has(span.path)) {
          fold(span.path);
          requestAnimationFrame(scroll);
        } else {
          scroll();
        }
      },
    }));

  const selection = dragged ?? editing;
  const quote = editing === null ? null : quoted(diff(editing.path), editing);

  return (
    <main className="changes" onMouseDown={keepFocus}>
      <CodeView<Extension>
        ref={view}
        className="code-view"
        items={items}
        options={options}
        selectedLines={
          selection === null
            ? null
            : { id: selection.path, range: range(selection) }
        }
        renderCodeViewHeader={() => (
          <>
            <Commits changes={changes} onMove={moved} />
            {files.length > 0 && (
              <Files
                files={files}
                onJump={(path) => {
                  jump(path);
                  if (folded.has(path)) {
                    fold(path);
                  }
                }}
              />
            )}
            <Overview entries={entries} />
            <div className="hint status">
              {files.length === 0
                ? `No changes since ${changes.since.slice(0, 7)}.`
                : notes.size === 0 && 'Tap or drag line numbers to comment.'}
            </div>
          </>
        )}
        // A file folded from its sticky header, scrolled far into it, would leave the
        // view among the files after it, so the view first scrolls to the file's top.
        renderHeaderPrefix={({ id }) => (
          <button
            className="fold"
            aria-label={folded.has(id) ? 'Unfold' : 'Fold'}
            onClick={() => {
              jump(id);
              fold(id);
            }}
          >
            {folded.has(id) ? '▸' : '▾'}
          </button>
        )}
        renderAnnotation={(...[{ metadata: extension }]) =>
          !('draft' in extension) ? (
            <Editor
              path={extension.path}
              label={quote === null ? '' : label(quote)}
              original={quote?.original ?? null}
              draft={draft}
              onDraft={setDraft}
              onSave={() => {
                if (quote === null) {
                  return;
                }
                const unchanged = draft.suggestion === quote.original;
                const span = extension;
                setNotes(
                  new Map(notes).set(key(span), {
                    span,
                    quote,
                    draft: {
                      text: draft.text,
                      suggestion: unchanged ? null : draft.suggestion,
                    },
                  }),
                );
                setEditing(null);
              }}
              onCancel={() => {
                setEditing(null);
              }}
            />
          ) : (
            <NoteView
              label={label(extension.quote)}
              path={extension.span.path}
              draft={extension.draft}
              original={extension.quote.original}
              onEdit={
                editing === null
                  ? () => {
                      setEditing(extension.span);
                      setDraft(extension.draft);
                    }
                  : null
              }
              onDelete={() => {
                const kept = new Map(notes);
                kept.delete(key(extension.span));
                setNotes(kept);
              }}
            />
          )
        }
      />
      <Sending count={notes.size} editing={editing !== null} review={review} />
    </main>
  );
}

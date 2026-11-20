import { useEffect, useMemo, useRef, useState } from 'react';
import { parsePatchFiles } from '@pierre/diffs';
import {
  CodeView,
  useStableCallback,
  type CodeViewDiffItem,
  type CodeViewHandle,
  type CodeViewReactOptions,
  type CodeViewScrollTarget,
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
  type Start,
  type Steps,
} from './api';
import {
  excerpt,
  isolated,
  real,
  rows,
  undone,
  type Point,
  type Row,
  type Shift,
} from './diff';
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

/**
 * A file the page shows, under an `id` of its own: its diff, the lines a message's
 * links name in it, and where the lines of its old side really are when it hides
 * changes. The diff is what the commit `step` changed, or without one, the changes of
 * the file as it is since the commit `since`, `null` for none to compare with.
 */
interface Shown {
  id: string;
  diff: FileDiffMetadata;
  marked: readonly Range[];
  shifts: readonly Shift[];
  since: string | null;
  step: Commit | null;
}

/** The changes since a start commit by commit, then those of the working tree. */
export interface Stepped {
  steps: Steps;
  tree: Opened;
}

/**
 * A file of the list atop the page, with what the page shows of it, `null` when the
 * visitor cannot read it.
 */
interface Listed {
  path: string;
  hint: string;
  shown: Shown | null;
}

/** The rows of the shown file `file` from `start` to `end`, in the order the view shows them. */
interface Span {
  file: string;
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

/** The span of `shown` between `one` and `other`, whichever the view shows first. */
function spanned({ id, diff }: Shown, one: Point, other: Point): Span {
  const all = rows(diff);
  const ends = [find(all, one), find(all, other)];
  const [start, end] = [all[Math.min(...ends)], all[Math.max(...ends)]];
  return start === undefined || end === undefined
    ? { file: id, start: one, end: other }
    : { file: id, start: point(start), end: point(end) };
}

/** What `span` covers of `shown`, whose lines a suggestion rewrites only as the file is. */
function quoted({ diff, step }: Shown, { start, end }: Span): Quote {
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
    original:
      old === null && step === null
        ? shown.map((row) => row.text).join('\n')
        : null,
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

const key = ({ file, end }: Span) =>
  `${file}\n${end.side}\n${String(end.line)}`;

function range({ start, end }: Span): SelectedLineRange {
  return {
    start: start.line,
    side: start.side,
    end: end.line,
    endSide: end.side,
  };
}

/**
 * The start of the changes, which a tap opens a sheet to move, and whether the page
 * shows them commit by commit, which a tap switches.
 */
function Commits({
  start,
  stepped,
  onMove,
  onStep,
}: {
  start: Start;
  stepped: boolean;
  onMove: (since: string) => void;
  onStep: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [head] = start.commits;
  const oldest = start.commits.find((commit) => commit.parent === start.since);
  const shown =
    start.since === head?.hash
      ? 'Working tree only'
      : (oldest?.subject ?? start.since.slice(0, 7));
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
      <button className="start" onClick={onStep}>
        <span>Commits</span>
        <span className="hint value">
          {stepped ? 'One by one' : 'Together'}
        </span>
      </button>
      {open && (
        <Starts
          start={start}
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
  start,
  onClose,
  onMove,
}: {
  start: Start;
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
    since: string | null,
    title: string,
    hint = '',
  ) => (
    <button
      key={key}
      className="cell"
      disabled={since === null}
      onClick={() => {
        onClose();
        if (since !== null && since !== start.since) {
          app.HapticFeedback.selectionChanged();
          onMove(since);
        }
      }}
    >
      <span className="grow">
        <span className="title">{title}</span>
        {hint && <span className="hint">{hint}</span>}
      </span>
      {since === start.since && <span className="check">✓</span>}
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
  const [head] = start.commits;
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
          start.commits.filter((commit) => commit.included),
        )}
        {group(
          'Earlier',
          start.commits.filter((commit) => !commit.included),
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
    .join('\u00a0');
}

/**
 * `lines` as short as a link's fragment writes them, since a file may list many. The
 * word joiner keeps a line from breaking after the dash.
 */
const brief = ({ from, to }: Range): string =>
  from === to ? `L${String(from)}` : `L${String(from)}–\u2060${String(to)}`;

/**
 * The files the page shows: those a message links to, with the lines its links name or
 * why the visitor cannot read one, or those the changes touch. A tap on one scrolls to it.
 */
function Files({
  files,
  onJump,
}: {
  files: readonly Listed[];
  onJump: (id: string) => void;
}) {
  return (
    <section className="section">
      <div className="label">Files</div>
      {files.map(({ path, hint, shown }) => (
        <button
          key={shown?.id ?? path}
          className="file"
          disabled={shown === null}
          onClick={() => {
            if (shown !== null) {
              onJump(shown.id);
            }
          }}
        >
          <span className="path">{path}</span>
          <span className="hint">{hint}</span>
        </button>
      ))}
    </section>
  );
}

/**
 * The files of a git `patch`, each with what `shown` makes of its diff and its hint. A
 * patch of some lines of a file hides the file's other changes, so each diff is
 * numbered as `isolated` has it.
 */
function patched(
  patch: string,
  shown: (diff: FileDiffMetadata, shifts: readonly Shift[]) => Shown,
  hinted: (counts: string) => string,
): Listed[] {
  const { patch: numbered, shifts } = isolated(patch);
  return parsePatchFiles(numbered)
    .flatMap((parsed) => parsed.files)
    .map((diff, index) => ({
      path:
        diff.prevName === undefined
          ? diff.name
          : `${diff.prevName} → ${diff.name}`,
      hint: hinted(counts(diff)),
      shown: shown(diff, shifts[index] ?? []),
    }));
}

/** What the page lists and shows of the files `opened` holds. */
function listed(opened: Opened): Listed[] {
  const since = opened.start?.since ?? null;
  if (opened.kind === 'changes') {
    return patched(
      opened.patch,
      (diff, shifts) => ({
        id: diff.name,
        diff,
        marked: [],
        shifts,
        since,
        step: null,
      }),
      (counts) => counts,
    );
  }
  return opened.files.map((file) => {
    if (!('content' in file)) {
      return { path: file.path, hint: file.refusal, shown: null };
    }
    // A link naming no lines shows the whole file.
    const whole = [{ from: 1, to: Infinity }];
    const { diff, shifts } = excerpt(
      { name: file.path, contents: file.old ?? file.content },
      { name: file.path, contents: file.content },
      file.lines.length > 0 ? file.lines : whole,
      opened.context,
    );
    const hint = [file.lines.map(brief).join(', '), counts(diff)];
    return {
      path: file.path,
      hint: hint.filter(Boolean).join(' ·\u00a0'),
      shown: {
        id: file.path,
        diff,
        marked: file.lines,
        shifts,
        since,
        step: null,
      },
    };
  });
}

/**
 * What the page lists and shows commit by commit: what each of `steps` changed, then
 * the files the working tree changed since `HEAD`, which `tree` holds.
 */
function stepwise({ steps, tree }: Stepped): Listed[] {
  const committed = steps.steps.flatMap(({ commit, patch }) =>
    patched(
      patch,
      (diff, shifts) => ({
        id: `${commit.hash}\n${diff.name}`,
        diff,
        marked: [],
        shifts,
        since: commit.parent,
        step: commit,
      }),
      (counts) => `${commit.hash.slice(0, 7)} · ${counts}`,
    ),
  );
  const changed = listed(tree).filter(
    ({ shown }) => shown === null || counts(shown.diff) !== '',
  );
  return [...committed, ...changed];
}

/**
 * Styles for the code, which the library draws in a shadow root of each file: the
 * page's background, and a tint on the lines the message's links name, under any
 * selection. A file's root carries its place among `files` once it renders.
 */
function codeStyle(files: readonly Shown[]): string {
  const rows = files.flatMap(({ marked }, index) =>
    marked.flatMap(({ from, to }) =>
      Array.from({ length: to - from + 1 }, (_, offset) => {
        const [host, line] = [String(index), String(from + offset)];
        return `:host([data-file="${host}"]) :is([data-line="${line}"], [data-column-number="${line}"]):not([data-line-type="change-deletion"])`;
      }),
    ),
  );
  const background =
    ':host { --diffs-light-bg: var(--bg); --diffs-dark-bg: var(--bg); }';
  return rows.length === 0
    ? background
    : `${background} ${rows.join(', ')} { --diffs-computed-decoration-bg: var(--target); }`;
}

export function Review({
  opened,
  stepped,
  onMove,
}: {
  opened: Opened;
  stepped: Stepped | null;
  onMove: (since: string, stepped: boolean) => void;
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
  const list = useMemo(
    () => (stepped === null ? listed(opened) : stepwise(stepped)),
    [opened, stepped],
  );
  const files = useMemo(() => list.flatMap(({ shown }) => shown ?? []), [list]);
  const shownBy = useMemo(
    () => new Map(files.map((shown) => [shown.id, shown])),
    [files],
  );
  const shown = (id: string) => {
    const found = shownBy.get(id);
    if (found === undefined) {
      throw new Error(`the page shows no ${id}`);
    }
    return found;
  };

  const since = opened.start?.since ?? null;
  // The lines of a commit's change are of the commit and of its parent, and any other
  // file's are of the file as it is and of its start. The prompt names removed lines by
  // their numbers in the file as of that commit.
  const review = () => ({
    notes: [...notes.values()].map(({ span, quote, draft }) => {
      const { diff, shifts, since, step } = shown(span.file);
      const [before, after] =
        step === null ? [since, null] : [step.parent, step.hash];
      return {
        new: quote.new && { commit: after, path: diff.name, ...quote.new },
        old: quote.old && {
          commit: before,
          path: diff.prevName ?? diff.name,
          from: real(shifts, quote.old.from),
          to: real(shifts, quote.old.to),
        },
        lines: quote.lines,
        text: draft.text.trim(),
        suggestion: draft.suggestion,
      };
    }),
  });

  const moved = (since: string, stepped: boolean) => {
    if (notes.size === 0 && editing === null) {
      onMove(since, stepped);
      return;
    }
    app.showConfirm('This drops your comments.', (confirmed) => {
      if (confirmed) {
        onMove(since, stepped);
      }
    });
  };

  // A tap on a line number opens the editor there, and a tap on another in the same file
  // while it is open stretches it over the lines between.
  const tapped = (file: string, at: Point) => {
    if (editing === null) {
      const here = spanned(shown(file), at, at);
      const note = notes.get(key(here));
      setEditing(note?.span ?? here);
      setDraft(note?.draft ?? EMPTY);
    } else if (editing.file === file) {
      setEditing(spanned(shown(file), editing.start, at));
    }
  };

  const started = useStableCallback(() => {
    dragging.current = false;
    setDragged(null);
  });
  const across = (file: string, range: SelectedLineRange): Span =>
    spanned(
      shown(file),
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
      const file = context.item.id;
      if (!drag) {
        tapped(file, { side: range.side ?? 'additions', line: range.start });
        return;
      }
      if (editing === null) {
        setDraft(EMPTY);
      }
      setEditing(across(file, range));
    },
  );

  // A file that came in a patch is loaded whole to expand it: its two sides, of which
  // the library hands back the diff it was given.
  const loaded = useStableCallback(async (fileDiff: FileDiffMetadata) => {
    const found = files.find(({ diff }) => diff === fileDiff);
    const prev = fileDiff.prevName ?? fileDiff.name;
    const query = new URLSearchParams({
      since: found?.since ?? '',
      path: fileDiff.name,
      prev,
    });
    if (found?.step != null) {
      query.set('at', found.step.hash);
    }
    try {
      const response = await call(`api/contents?${query.toString()}`);
      const contents = (await response.json()) as { old: string; new: string };
      const newFile = { name: fileDiff.name, contents: contents.new };
      // The library takes no old side for a file renamed and left as it was.
      if (fileDiff.type === 'rename-pure') {
        return { oldFile: null, newFile };
      }
      const old = undone(
        contents.old,
        contents.new,
        fileDiff,
        found?.shifts ?? [],
      );
      return { oldFile: { name: prev, contents: old }, newFile };
    } catch (error) {
      app.showAlert(failure(error));
      throw error;
    }
  });

  // A pooled root may have drawn another file before, so each render marks it afresh.
  const rendered = useStableCallback(
    (
      node: HTMLElement,
      _instance: unknown,
      _phase: unknown,
      context: { item: { id: string } },
    ) => {
      node.dataset.file = String(
        files.findIndex(({ id }) => id === context.item.id),
      );
    },
  );

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
      onPostRender: rendered,
      unsafeCSS: codeStyle(files),
    }),
    [scheme, loaded, started, changed, selected, rendered, files],
  );

  const items = useMemo(
    () =>
      files.map(({ id, diff: fileDiff }): CodeViewDiffItem<Extension> => {
        const saved = [...notes.values()].filter(
          (note) =>
            note.span.file === id &&
            (editing === null || key(note.span) !== key(editing)),
        );
        const at = ({ side, line }: Point) => ({ side, lineNumber: line });
        const annotations: DiffLineAnnotation<Extension>[] = [
          ...(editing?.file === id
            ? [{ ...at(editing.end), metadata: editing }]
            : []),
          ...saved.map((note) => ({ ...at(note.span.end), metadata: note })),
        ];
        const collapsed = folded.has(id);
        return {
          id,
          type: 'diff',
          fileDiff,
          annotations,
          collapsed,
          version: version(fileDiff, annotations, collapsed),
        };
      }),
    [files, notes, editing, folded],
  );

  const fold = (id: string) => {
    const next = new Set(folded);
    if (!next.delete(id)) {
      next.add(id);
    }
    setFolded(next);
  };
  const jump = (id: string) => {
    view.current?.scrollTo({ type: 'item', id, align: 'start' });
  };
  const reveal = (target: CodeViewScrollTarget & { id: string }) => {
    const scroll = () => {
      view.current?.scrollTo(target);
    };
    // The lines of a folded file have no place to scroll to until it unfolds.
    if (folded.has(target.id)) {
      fold(target.id);
      requestAnimationFrame(scroll);
    } else {
      scroll();
    }
  };

  // A page of files opens at the file and lines of the link tapped, once the view has
  // drawn.
  useEffect(() => {
    if (opened.kind !== 'files' || stepped !== null) {
      return;
    }
    const target = list[opened.file]?.shown;
    if (target === undefined || target === null) {
      return;
    }
    const id = target.id;
    requestAnimationFrame(() => {
      view.current?.scrollTo(
        opened.lines === null
          ? { type: 'item', id, align: 'start' }
          : {
              type: 'line',
              id,
              lineNumber: opened.lines.from,
              side: 'additions',
              align: 'center',
            },
      );
    });
  }, [opened, stepped, list]);

  /** The path of a shown file, behind the commit whose change it is. */
  const named = ({ diff, step }: Shown) =>
    step === null ? diff.name : `${step.hash.slice(0, 7)}:${diff.name}`;
  const order = (note: Note): [number, number] => [
    files.findIndex(({ id }) => id === note.span.file),
    find(rows(shown(note.span.file).diff), note.span.end),
  ];
  const entries = [...notes.values()]
    .map((note) => ({ note, at: order(note) }))
    .sort(
      ({ at: [file, row] }, { at: [otherFile, otherRow] }) =>
        file - otherFile || row - otherRow,
    )
    .map(({ note: { span, quote, draft } }) => ({
      key: key(span),
      label: `${named(shown(span.file))} · ${label(quote)}`,
      draft,
      onJump: () => {
        reveal({
          type: 'line',
          id: span.file,
          lineNumber: span.end.line,
          side: span.end.side,
          align: 'center',
        });
      },
    }));

  const selection = dragged ?? editing;
  const quote = editing === null ? null : quoted(shown(editing.file), editing);

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
            : { id: selection.file, range: range(selection) }
        }
        renderCodeViewHeader={() => (
          <>
            {opened.start !== null && (
              <Commits
                start={opened.start}
                stepped={stepped !== null}
                onMove={(since) => {
                  moved(since, stepped !== null);
                }}
                onStep={() => {
                  if (since !== null) {
                    moved(since, stepped === null);
                  }
                }}
              />
            )}
            {(list.length > 1 || files.length < list.length) && (
              <Files
                files={list}
                onJump={(id) => {
                  reveal({ type: 'item', id, align: 'start' });
                }}
              />
            )}
            <Overview entries={entries} />
            <div className="hint status">
              {list.length === 0 && since !== null
                ? `No changes since ${since.slice(0, 7)}.`
                : stepped?.steps.more === true
                  ? `Only the latest ${String(stepped.steps.steps.length)} commits are shown.`
                  : files.length > 0 &&
                    notes.size === 0 &&
                    'Tap or drag line numbers to comment.'}
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
        // In the changes commit by commit, a file's header says whose change it is.
        renderHeaderMetadata={({ id }) => {
          const step = stepped === null ? null : shown(id).step;
          return (
            stepped !== null && (
              <span className="hint step">
                {step === null
                  ? 'Working tree'
                  : `${step.hash.slice(0, 7)} ${step.subject}`}
              </span>
            )
          );
        }}
        renderAnnotation={(...[{ metadata: extension }]) =>
          !('draft' in extension) ? (
            <Editor
              path={shown(extension.file).diff.name}
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
              path={shown(extension.span.file).diff.name}
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

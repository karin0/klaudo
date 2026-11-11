import { useEffect, useMemo, useRef, useState } from 'react';
import {
  CodeView,
  useStableCallback,
  type CodeViewFileItem,
  type CodeViewHandle,
  type CodeViewReactOptions,
  type CodeViewScrollTarget,
  type LineAnnotation,
  type SelectedLineRange,
} from '@pierre/diffs/react';
import type { Linked, Opened, Range } from './api';
import {
  EMPTY,
  Editor,
  NoteView,
  Overview,
  Sending,
  THEMES,
  keepFocus,
  span,
  useKeyboardReveal,
  useScheme,
  version,
  type Draft,
} from './notes';

type Shown = Extract<Opened, { kind: 'files' }>;

type Readable = Extract<Linked, { content: string }>;

/** Lines of the file at `path`. */
interface Span extends Range {
  path: string;
}

type Note = Span & Draft;

/** What a line carries under it: a saved note, or the span the editor is open on. */
type Extension = Note | Span;

const key = ({ path, to }: Span) => `${path}\n${String(to)}`;

const readable = (file: Linked): file is Readable => 'content' in file;

/** Where the page scrolls to show `line` of the file at `path`, or the file's top for `null`. */
const place = (path: string, line: number | null): CodeViewScrollTarget =>
  line === null
    ? { type: 'item', id: path, align: 'start' }
    : { type: 'line', id: path, lineNumber: line, align: 'center' };

function ordered(path: string, { start, end }: SelectedLineRange): Span {
  return { path, from: Math.min(start, end), to: Math.max(start, end) };
}

/**
 * Styles for the code, which the library draws in a shadow root of each file: the
 * page's background, and a tint on the lines the message's links point at, under any
 * selection. A file's root carries its place among `files` once it renders.
 */
function codeStyle(files: readonly Readable[]): string {
  const rows = files.flatMap((file, index) =>
    file.lines.flatMap(({ from, to }) =>
      Array.from({ length: to - from + 1 }, (_, offset) => {
        const [host, line] = [String(index), String(from + offset)];
        return [
          `:host([data-file="${host}"]) [data-line="${line}"]`,
          `:host([data-file="${host}"]) [data-column-number="${line}"]`,
        ];
      }).flat(),
    ),
  );
  const background =
    ':host { --diffs-light-bg: var(--bg); --diffs-dark-bg: var(--bg); }';
  return rows.length === 0
    ? background
    : `${background} ${rows.join(', ')} { --diffs-computed-decoration-bg: var(--target); }`;
}

/** The files the message links to, each scrolling the page to itself on a tap, or saying why it cannot be read. */
function Files({
  files,
  onJump,
}: {
  files: readonly Linked[];
  onJump: (path: string) => void;
}) {
  return (
    <section className="section">
      <div className="label">Files</div>
      {files.map((file) => (
        <button
          key={file.path}
          className="file"
          disabled={!readable(file)}
          onClick={() => {
            onJump(file.path);
          }}
        >
          <span className="path">{file.path}</span>
          <span className="hint">
            {readable(file)
              ? file.lines.map((lines) => span(lines)).join(', ')
              : file.refusal}
          </span>
        </button>
      ))}
    </section>
  );
}

export function FilesReview({ shown }: { shown: Shown }) {
  const scheme = useScheme();
  useKeyboardReveal();
  const [notes, setNotes] = useState<ReadonlyMap<string, Note>>(new Map());
  const [editing, setEditing] = useState<Span | null>(null);
  // The lines under a finger dragging across line numbers, which become `editing` when
  // it lifts. The ref tells a drag from a tap before the drag's last move has rendered.
  const [dragged, setDragged] = useState<Span | null>(null);
  const dragging = useRef(false);
  const [draft, setDraft] = useState<Draft>(EMPTY);
  const [folded, setFolded] = useState<ReadonlySet<string>>(new Set());
  const view = useRef<CodeViewHandle<Extension, undefined>>(null);

  const files = useMemo(() => shown.files.filter(readable), [shown]);
  // The code view would draw the empty text after a final line break as one more line.
  const code = useMemo(
    () =>
      new Map(
        files.map((file) => [
          file.path,
          { name: file.path, contents: file.content.replace(/\n$/, '') },
        ]),
      ),
    [files],
  );
  const lines = useMemo(
    () =>
      new Map(
        [...code].map(([path, { contents }]) => [path, contents.split('\n')]),
      ),
    [code],
  );
  const original = ({ path, from, to }: Span) =>
    (lines.get(path) ?? []).slice(from - 1, to).join('\n');

  const sorted = [...notes.values()].sort(
    (one, other) =>
      files.findIndex((file) => file.path === one.path) -
        files.findIndex((file) => file.path === other.path) ||
      one.from - other.from,
  );
  const review = () => ({
    since: null,
    notes: sorted.map((note) => ({
      new: { path: note.path, from: note.from, to: note.to },
      old: null,
      lines: original(note)
        .split('\n')
        .map((line) => ` ${line}`),
      text: note.text.trim(),
      suggestion: note.suggestion,
    })),
  });

  // A tap on a line number opens the editor there, and a tap on another in the same file
  // while it is open stretches it over the lines between.
  const tapped = (path: string, line: number) => {
    if (editing === null) {
      const note = notes.get(key({ path, from: line, to: line }));
      setEditing({ path, from: note?.from ?? line, to: line });
      setDraft(note ?? EMPTY);
    } else if (editing.path === path) {
      setEditing({
        path,
        from: Math.min(editing.from, line),
        to: Math.max(editing.from, line),
      });
    }
  };

  const started = useStableCallback(() => {
    dragging.current = false;
    setDragged(null);
  });
  const changed = useStableCallback(
    (range: SelectedLineRange | null, context: { item: { id: string } }) => {
      dragging.current = true;
      setDragged(range === null ? null : ordered(context.item.id, range));
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
        tapped(path, range.start);
        return;
      }
      if (editing === null) {
        setDraft(EMPTY);
      }
      setEditing(ordered(path, range));
    },
  );
  // A pooled root may have drawn another file before, so each render marks it afresh.
  const rendered = useStableCallback(
    (
      node: HTMLElement,
      _instance: unknown,
      _phase: unknown,
      context: { item: { id: string } },
    ) => {
      node.dataset.file = String(
        files.findIndex((file) => file.path === context.item.id),
      );
    },
  );

  const options = useMemo(
    (): CodeViewReactOptions<Extension, undefined> => ({
      theme: THEMES,
      themeType: scheme,
      overflow: 'wrap',
      stickyHeaders: true,
      enableLineSelection: true,
      onLineSelectionStart: started,
      onLineSelectionChange: changed,
      onLineSelectionEnd: selected,
      onPostRender: rendered,
      unsafeCSS: codeStyle(files),
    }),
    [scheme, files, started, changed, selected, rendered],
  );

  const items = useMemo(
    () =>
      [...code].map(([path, file]): CodeViewFileItem<Extension> => {
        const saved = [...notes.values()].filter(
          (note) =>
            note.path === path &&
            (editing === null || key(note) !== key(editing)),
        );
        const annotations: LineAnnotation<Extension>[] = [
          ...(editing?.path === path
            ? [{ lineNumber: editing.to, metadata: editing }]
            : []),
          ...saved.map((note) => ({ lineNumber: note.to, metadata: note })),
        ];
        const collapsed = folded.has(path);
        return {
          id: path,
          type: 'file',
          file,
          annotations,
          collapsed,
          version: version(file, annotations, collapsed),
        };
      }),
    [code, notes, editing, folded],
  );

  const fold = (path: string) => {
    const next = new Set(folded);
    if (!next.delete(path)) {
      next.add(path);
    }
    setFolded(next);
  };
  const reveal = (path: string, line: number | null) => {
    const scroll = () => {
      view.current?.scrollTo(place(path, line));
    };
    // The lines of a folded file have no place to scroll to until it unfolds.
    if (folded.has(path)) {
      fold(path);
      requestAnimationFrame(scroll);
    } else {
      scroll();
    }
  };

  // The page opens at the file and lines of the link tapped, once the view has drawn.
  useEffect(() => {
    const target = shown.files[shown.file];
    if (target !== undefined && readable(target)) {
      requestAnimationFrame(() => {
        view.current?.scrollTo(place(target.path, shown.lines?.from ?? null));
      });
    }
  }, [shown]);

  const entries = sorted.map((note) => ({
    key: key(note),
    label: `${note.path} · ${span(note)}`,
    draft: note,
    onJump: () => {
      reveal(note.path, note.to);
    },
  }));

  const selection = dragged ?? editing;

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
            : {
                id: selection.path,
                range: { start: selection.from, end: selection.to },
              }
        }
        renderCodeViewHeader={() => (
          <>
            {(shown.files.length > 1 || files.length === 0) && (
              <Files
                files={shown.files}
                onJump={(path) => {
                  reveal(path, null);
                }}
              />
            )}
            <Overview entries={entries} />
            {files.length > 0 && notes.size === 0 && (
              <div className="hint status">
                Tap or drag line numbers to comment.
              </div>
            )}
          </>
        )}
        // A file folded from its sticky header, scrolled far into it, would leave the
        // view among the files after it, so the view first scrolls to the file's top.
        renderHeaderPrefix={({ id }) => (
          <button
            className="fold"
            aria-label={folded.has(id) ? 'Unfold' : 'Fold'}
            onClick={() => {
              view.current?.scrollTo({ type: 'item', id, align: 'start' });
              fold(id);
            }}
          >
            {folded.has(id) ? '▸' : '▾'}
          </button>
        )}
        renderAnnotation={(...[{ metadata: extension }]) =>
          !('text' in extension) ? (
            <Editor
              path={extension.path}
              label={span(extension)}
              original={original(extension)}
              draft={draft}
              onDraft={setDraft}
              onSave={() => {
                const unchanged = draft.suggestion === original(extension);
                setNotes(
                  new Map(notes).set(key(extension), {
                    ...extension,
                    text: draft.text,
                    suggestion: unchanged ? null : draft.suggestion,
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
              label={span(extension)}
              path={extension.path}
              draft={extension}
              original={original(extension)}
              onEdit={
                editing === null
                  ? () => {
                      const { path, from, to } = extension;
                      setEditing({ path, from, to });
                      setDraft(extension);
                    }
                  : null
              }
              onDelete={() => {
                const kept = new Map(notes);
                kept.delete(key(extension));
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

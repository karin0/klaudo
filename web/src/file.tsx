import { useMemo, useRef, useState } from 'react';
import {
  File,
  useStableCallback,
  type FileOptions,
  type LineAnnotation,
  type SelectedLineRange,
} from '@pierre/diffs/react';
import type { Opened, Range } from './api';
import {
  EMPTY,
  Editor,
  NoteView,
  Overview,
  THEMES,
  keepFocus,
  span,
  useKeyboardReveal,
  useScheme,
  useSending,
  type Draft,
} from './notes';

type Shown = Extract<Opened, { kind: 'file' }>;

type Note = Range & Draft;

/** What a line carries under it: a saved note, or the editor of the lines being commented on. */
type Extension = { note: Note } | { editor: Range };

function ordered({ start, end }: SelectedLineRange): Range {
  return { from: Math.min(start, end), to: Math.max(start, end) };
}

/**
 * Styles for the code, which the library draws in a shadow root: the page's background,
 * and a tint on the lines the link points at, under any selection.
 */
function codeStyle({ from, to }: Range): string {
  const background =
    ':host { --diffs-light-bg: var(--bg); --diffs-dark-bg: var(--bg); }';
  if (from === 0) {
    return background;
  }
  const rows = Array.from({ length: to - from + 1 }, (_, index) => {
    const line = String(from + index);
    return `[data-line="${line}"], [data-column-number="${line}"]`;
  });
  return `${background} ${rows.join(', ')} { --diffs-computed-decoration-bg: var(--target); }`;
}

export function FileReview({ shown }: { shown: Shown }) {
  const scheme = useScheme();
  useKeyboardReveal();
  const [notes, setNotes] = useState<ReadonlyMap<number, Note>>(new Map());
  const [editing, setEditing] = useState<Range | null>(null);
  // The lines under a finger dragging across line numbers, which become `editing` when
  // it lifts. The ref tells a drag from a tap before the drag's last move has rendered.
  const [dragged, setDragged] = useState<Range | null>(null);
  const dragging = useRef(false);
  const [draft, setDraft] = useState<Draft>(EMPTY);

  const lines = useMemo(
    () => shown.content.replace(/\n$/, '').split('\n'),
    [shown],
  );
  const original = ({ from, to }: Range) => lines.slice(from - 1, to);
  const file = useMemo(
    () => ({ name: shown.path, contents: shown.content }),
    [shown],
  );

  const sorted = [...notes.values()].sort(
    (one, other) => one.from - other.from,
  );
  useSending(notes.size, editing !== null, () => ({
    since: null,
    notes: sorted.map((note) => ({
      new: { path: shown.path, from: note.from, to: note.to },
      old: null,
      lines: original(note).map((line) => ` ${line}`),
      text: note.text.trim(),
      suggestion: note.suggestion,
    })),
  }));

  // A tap on a line number opens the editor there, and a tap on another while it is
  // open stretches it over the lines between.
  const tapped = (line: number) => {
    if (editing === null) {
      const note = notes.get(line);
      setEditing({ from: note?.from ?? line, to: line });
      setDraft(note ?? EMPTY);
    } else {
      setEditing({
        from: Math.min(editing.from, line),
        to: Math.max(editing.from, line),
      });
    }
  };

  const started = useStableCallback(() => {
    dragging.current = false;
    setDragged(null);
  });
  const changed = useStableCallback((range: SelectedLineRange | null) => {
    dragging.current = true;
    setDragged(range === null ? null : ordered(range));
  });
  // The library reports a tap on the one line selected as `null`, which leaves the
  // editor open, since only its Cancel button drops the draft.
  const selected = useStableCallback((range: SelectedLineRange | null) => {
    const drag = dragging.current;
    started();
    if (range === null) {
      return;
    }
    if (!drag) {
      tapped(range.start);
      return;
    }
    if (editing === null) {
      setDraft(EMPTY);
    }
    setEditing(ordered(range));
  });
  // The library draws the lines after the render it reports as the mount, so the page
  // jumps to the target on the first render that has it.
  const jumped = useRef(shown.from === 0);
  const code = useRef<HTMLElement | null>(null);
  const rendered = useStableCallback((node: HTMLElement) => {
    code.current = node;
    const target = node.shadowRoot?.querySelector(
      `[data-line="${String(shown.from)}"]`,
    );
    if (!jumped.current && target != null) {
      jumped.current = true;
      target.scrollIntoView({ block: 'center' });
    }
  });

  const options = useMemo(
    (): FileOptions<Extension, undefined> => ({
      theme: THEMES,
      themeType: scheme,
      overflow: 'wrap',
      disableFileHeader: true,
      enableLineSelection: true,
      onLineSelectionStart: started,
      onLineSelectionChange: changed,
      onLineSelectionEnd: selected,
      onPostRender: rendered,
      unsafeCSS: codeStyle(shown),
    }),
    [scheme, shown, started, changed, selected, rendered],
  );

  const annotations = useMemo((): LineAnnotation<Extension>[] => {
    const saved = [...notes.values()]
      .filter((note) => note.to !== editing?.to)
      .map((note) => ({ lineNumber: note.to, metadata: { note } }));
    return editing === null
      ? saved
      : [{ lineNumber: editing.to, metadata: { editor: editing } }, ...saved];
  }, [notes, editing]);

  const entries = sorted.map((note) => ({
    key: String(note.to),
    label: span(note),
    draft: note,
    onJump: () => {
      code.current?.shadowRoot
        ?.querySelector(`[data-line="${String(note.to)}"]`)
        ?.scrollIntoView({ block: 'center' });
    },
  }));

  const selection = dragged ?? editing;

  return (
    <>
      <header>
        {shown.path}
        {shown.from > 0 && <span className="hint"> {span(shown)}</span>}
        {notes.size === 0 && (
          <div className="hint">Tap or drag line numbers to comment.</div>
        )}
      </header>
      <main onMouseDown={keepFocus}>
        <Overview entries={entries} />
        <File<Extension>
          file={file}
          options={options}
          lineAnnotations={annotations}
          selectedLines={
            selection === null
              ? null
              : { start: selection.from, end: selection.to }
          }
          renderAnnotation={({ metadata: extension }) =>
            'editor' in extension ? (
              <Editor
                path={shown.path}
                label={span(extension.editor)}
                original={original(extension.editor).join('\n')}
                draft={draft}
                onDraft={setDraft}
                onSave={() => {
                  const unchanged =
                    draft.suggestion === original(extension.editor).join('\n');
                  setNotes(
                    new Map(notes).set(extension.editor.to, {
                      ...extension.editor,
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
                label={span(extension.note)}
                path={shown.path}
                draft={extension.note}
                original={original(extension.note).join('\n')}
                onEdit={
                  editing === null
                    ? () => {
                        const { note } = extension;
                        setEditing({ from: note.from, to: note.to });
                        setDraft(note);
                      }
                    : null
                }
                onDelete={() => {
                  const kept = new Map(notes);
                  kept.delete(extension.note.to);
                  setNotes(kept);
                }}
              />
            )
          }
        />
      </main>
    </>
  );
}

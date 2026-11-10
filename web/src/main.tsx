import { StrictMode, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  File,
  useStableCallback,
  type FileOptions,
  type LineAnnotation,
  type SelectedLineRange,
} from '@pierre/diffs/react';
import './app.css';

const app = window.Telegram.WebApp;
app.ready();
app.expand();

/** What `GET /api/file` answers: the file a link names and the lines it points at, 0 for none. */
interface Shown {
  path: string;
  content: string;
  from: number;
  to: number;
}

interface Range {
  from: number;
  to: number;
}

interface Draft {
  text: string;
  /** The lines as the reviewer would write them, or `null` for a comment alone. */
  suggestion: string | null;
}

type Note = Range & Draft;

/** What a line carries under it: a saved note, or the editor of the lines being commented on. */
type Extension = { note: Note } | { editor: Range };

const EMPTY: Draft = { text: '', suggestion: null };

const failure = (error: unknown) =>
  error instanceof Error ? error.message : String(error);

/** A request to the daemon, which reads who asks and what they may see from `initData`. */
async function call(path: string, body?: unknown): Promise<Response> {
  const response = await fetch(path, {
    method: body === undefined ? 'GET' : 'POST',
    headers: {
      Authorization: `tma ${app.initData}`,
      'Content-Type': 'application/json',
    },
    body: body === undefined ? null : JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error((await response.text()) || response.statusText);
  }
  return response;
}

function useScheme(): 'light' | 'dark' {
  const [scheme, setScheme] = useState(app.colorScheme);
  useEffect(() => {
    const changed = () => {
      setScheme(app.colorScheme);
    };
    app.onEvent('themeChanged', changed);
    return () => {
      app.offEvent('themeChanged', changed);
    };
  }, []);
  return scheme;
}

/** Scrolls the focused field into view once the viewport settles, as after the keyboard opens. */
function useKeyboardReveal() {
  useEffect(() => {
    const changed = ({ isStateStable }: { isStateStable: boolean }) => {
      const field = document.activeElement;
      if (isStateStable && field instanceof HTMLTextAreaElement) {
        field.scrollIntoView({ block: 'nearest' });
      }
    };
    app.onEvent('viewportChanged', changed);
    return () => {
      app.offEvent('viewportChanged', changed);
    };
  }, []);
}

function span({ from, to }: Range): string {
  return from === to
    ? `Line ${String(from)}`
    : `Lines ${String(from)}–${String(to)}`;
}

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

function Editor({
  range,
  original,
  draft,
  onDraft,
  onSave,
  onCancel,
}: {
  range: Range;
  original: string;
  draft: Draft;
  onDraft: (draft: Draft) => void;
  onSave: () => void;
  onCancel: () => void;
}) {
  const { text, suggestion } = draft;
  const empty =
    !text.trim() && (suggestion === null || suggestion === original);
  const box = useRef<HTMLDivElement>(null);
  const suggesting = suggestion !== null;
  // The library moves the editor into place in its own layout effect, after React has
  // mounted it, so `autoFocus` would find the fields detached.
  useEffect(() => {
    const field = box.current?.querySelector('textarea');
    field?.focus();
    field?.setSelectionRange(field.value.length, field.value.length);
  }, [suggesting]);
  return (
    <div ref={box} className="extension editor">
      <div className="label">{span(range)}</div>
      {suggestion !== null && (
        <textarea
          className="code"
          value={suggestion}
          onChange={(event) => {
            onDraft({ text, suggestion: event.target.value });
          }}
        />
      )}
      <textarea
        value={text}
        placeholder={
          suggestion === null ? 'Leave a comment' : 'Why this change (optional)'
        }
        rows={3}
        onChange={(event) => {
          onDraft({ text: event.target.value, suggestion });
        }}
      />
      <div className="buttons">
        <button
          onClick={() => {
            onDraft({
              text,
              suggestion: suggestion === null ? original : null,
            });
          }}
        >
          {suggestion === null ? 'Suggest change' : 'Drop suggestion'}
        </button>
        <span className="grow" />
        <button onClick={onCancel}>Cancel</button>
        <button className="primary" disabled={empty} onClick={onSave}>
          Save
        </button>
      </div>
    </div>
  );
}

function NoteView({
  note,
  original,
  onEdit,
  onDelete,
}: {
  note: Note;
  original: readonly string[];
  /** Opens the note in the editor, or `null` while the editor is open on other lines. */
  onEdit: (() => void) | null;
  onDelete: () => void;
}) {
  return (
    <div className="extension note">
      <div className="label">{span(note)}</div>
      {note.suggestion !== null && (
        <pre className="suggestion">
          {original.map((line, index) => (
            <div key={`-${String(index)}`} className="removed">
              - {line}
            </div>
          ))}
          {note.suggestion.split('\n').map((line, index) => (
            <div key={`+${String(index)}`} className="added">
              + {line}
            </div>
          ))}
        </pre>
      )}
      {note.text && <div className="text">{note.text}</div>}
      <div className="buttons">
        <span className="grow" />
        <button disabled={onEdit === null} onClick={onEdit ?? undefined}>
          Edit
        </button>
        <button className="danger" onClick={onDelete}>
          Delete
        </button>
      </div>
    </div>
  );
}

function Review({ shown }: { shown: Shown }) {
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

  useEffect(() => {
    if (notes.size > 0) {
      app.enableClosingConfirmation();
    } else {
      app.disableClosingConfirmation();
    }
  }, [notes]);

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
  const rendered = useStableCallback((node: HTMLElement) => {
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
      theme: { dark: 'one-dark-pro', light: 'one-light' },
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

  const selection = dragged ?? editing;

  const send = useStableCallback(async () => {
    app.MainButton.showProgress();
    try {
      await call(
        'api/review',
        [...notes.values()]
          .sort((one, other) => one.from - other.from)
          .map((note) => ({
            path: shown.path,
            from: note.from,
            to: note.to,
            original: original(note).join('\n'),
            text: note.text.trim(),
            suggestion: note.suggestion,
          })),
      );
      setNotes(new Map());
      app.disableClosingConfirmation();
      app.close();
    } catch (error) {
      app.showAlert(failure(error));
    } finally {
      app.MainButton.hideProgress();
    }
  });

  useEffect(() => {
    const clicked = () => {
      void send();
    };
    app.MainButton.onClick(clicked);
    return () => {
      app.MainButton.offClick(clicked);
    };
  }, [send]);

  useEffect(() => {
    app.MainButton.setParams({
      text: `Send review (${String(notes.size)})`,
      // Hidden while the editor is open, where the keyboard would lift it over the
      // editor, and a send would drop the draft.
      is_visible: notes.size > 0 && editing === null,
    });
  }, [notes, editing]);

  return (
    <>
      <header>
        {shown.path}
        {shown.from > 0 && <span className="hint"> {span(shown)}</span>}
        {notes.size === 0 && (
          <div className="hint">Tap or drag line numbers to comment.</div>
        )}
      </header>
      <main
        onMouseDown={(event) => {
          // The mouse event a browser fires after a tap would move the focus from the
          // editor the tap opened to the line number tapped.
          const number = event.nativeEvent
            .composedPath()
            .some(
              (node) =>
                node instanceof Element &&
                node.hasAttribute('data-column-number'),
            );
          if (number) {
            event.preventDefault();
          }
        }}
      >
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
                range={extension.editor}
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
                note={extension.note}
                original={original(extension.note)}
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

function App() {
  const [shown, setShown] = useState<Shown | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);
  useEffect(() => {
    call('api/file')
      .then(async (response) => (await response.json()) as Shown)
      .then(setShown, (error: unknown) => {
        setRefusal(failure(error));
      });
  }, []);
  if (shown === null) {
    return <p className="hint status">{refusal ?? 'Loading…'}</p>;
  }
  return <Review shown={shown} />;
}

const root = document.getElementById('root');
if (root === null) {
  throw new Error('index.html has no #root');
}
createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);

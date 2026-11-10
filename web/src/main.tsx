import {
  StrictMode,
  useEffect,
  useMemo,
  useState,
  type MouseEvent,
} from 'react';
import { createRoot } from 'react-dom/client';
import { DiffModeEnum, DiffView } from '@git-diff-view/react';
import '@git-diff-view/react/styles/diff-view-pure.css';
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

/** A rule painting the rows of `range`, whose number the library writes on each row. */
function paint(range: Range | null, colour: string): string {
  if (range === null || range.from === 0) {
    return '';
  }
  const rows = Array.from(
    { length: range.to - range.from + 1 },
    (_, index) => `tr[data-line="${String(range.from + index)}"] > td`,
  );
  return `${rows.join(',')} { background: ${colour} !important; }`;
}

function span({ from, to }: Range): string {
  return from === to
    ? `Line ${String(from)}`
    : `Lines ${String(from)}–${String(to)}`;
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
  return (
    <div className="card editor">
      <div className="hint">{span(range)}</div>
      {/* DiffView takes a new renderExtendLine in an effect, so a controlled value would
          trail each keystroke by a render, which moves the caret and breaks IME input. */}
      {suggestion !== null && (
        <textarea
          className="code"
          defaultValue={suggestion}
          onChange={(event) => {
            onDraft({ text, suggestion: event.target.value });
          }}
          autoFocus
        />
      )}
      <textarea
        defaultValue={text}
        placeholder={
          suggestion === null ? 'Leave a comment' : 'Why this change (optional)'
        }
        rows={3}
        onChange={(event) => {
          onDraft({ text: event.target.value, suggestion });
        }}
        autoFocus={suggestion === null}
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
  onDelete,
}: {
  note: Note;
  original: readonly string[];
  onDelete: () => void;
}) {
  return (
    <div className="card">
      <div className="buttons">
        <span className="hint grow">{span(note)}</span>
        <button onClick={onDelete}>Delete</button>
      </div>
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
    </div>
  );
}

function Review({ shown }: { shown: Shown }) {
  const scheme = useScheme();
  const [notes, setNotes] = useState<ReadonlyMap<number, Note>>(new Map());
  const [editing, setEditing] = useState<Range | null>(null);
  const [draft, setDraft] = useState<Draft>({ text: '', suggestion: null });
  const [sending, setSending] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);

  const lines = useMemo(
    () => shown.content.replace(/\n$/, '').split('\n'),
    [shown],
  );
  const original = ({ from, to }: Range) => lines.slice(from - 1, to);
  // The library draws only hunks, so the whole file is one hunk of context lines.
  const data = useMemo(() => {
    const count = String(lines.length);
    const hunk = [
      `--- a/${shown.path}`,
      `+++ b/${shown.path}`,
      `@@ -1,${count} +1,${count} @@`,
      ...lines.map((line) => ` ${line}`),
    ].join('\n');
    const side = {
      fileName: shown.path,
      fileLang: /\.([^./]+)$/.exec(shown.path)?.[1] ?? 'plaintext',
      content: shown.content,
    };
    return { oldFile: side, newFile: side, hunks: [hunk] };
  }, [shown, lines]);

  useEffect(() => {
    if (shown.from > 0) {
      requestAnimationFrame(() => {
        document
          .querySelector(`tr[data-line="${String(shown.from)}"]`)
          ?.scrollIntoView({ block: 'center' });
      });
    }
  }, [shown]);

  useEffect(() => {
    if (notes.size > 0) {
      app.enableClosingConfirmation();
    } else {
      app.disableClosingConfirmation();
    }
  }, [notes]);

  // A tap on a line number opens the editor there, and a tap on another while it is
  // open stretches it over the lines between.
  const tapped = (event: MouseEvent) => {
    if (!(event.target instanceof Element)) {
      return;
    }
    const number = event.target
      .closest('td.diff-line-num')
      ?.querySelector('[data-line-new-num]')
      ?.getAttribute('data-line-new-num');
    if (number == null) {
      return;
    }
    const line = Number(number);
    if (editing === null) {
      const note = notes.get(line);
      setEditing({ from: note?.from ?? line, to: line });
      setDraft(note ?? { text: '', suggestion: null });
    } else if (editing.from === line && editing.to === line) {
      setEditing(null);
    } else {
      setEditing({
        from: Math.min(editing.from, line),
        to: Math.max(editing.from, line),
      });
    }
  };

  const extensions: Record<string, { data: Extension }> = {};
  for (const note of notes.values()) {
    extensions[String(note.to)] = { data: { note } };
  }
  if (editing !== null) {
    extensions[String(editing.to)] = { data: { editor: editing } };
  }

  const send = async () => {
    setSending(true);
    setRefusal(null);
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
      setRefusal(failure(error));
    } finally {
      setSending(false);
    }
  };

  return (
    <>
      <style>
        {paint(shown, 'var(--target)')}
        {paint(editing, 'var(--selected)')}
      </style>
      <header>
        {shown.path}
        {shown.from > 0 && <span className="hint"> {span(shown)}</span>}
      </header>
      <main onClick={tapped}>
        <DiffView<Extension>
          data={data}
          diffViewMode={DiffModeEnum.Unified}
          diffViewTheme={scheme}
          diffViewHighlight
          diffViewWrap
          diffViewFontSize={13}
          extendData={{ newFile: extensions }}
          renderExtendLine={({ data: extension }) =>
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
      <footer>
        <span className="hint grow">
          {refusal ?? 'Tap a line number to comment, and another for a range.'}
        </span>
        <button
          className="primary"
          disabled={notes.size === 0 || sending}
          onClick={() => {
            void send();
          }}
        >
          Send review{notes.size > 0 && ` (${String(notes.size)})`}
        </button>
      </footer>
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

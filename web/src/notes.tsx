import { useEffect, useRef, useState } from 'react';
import { useStableCallback } from '@pierre/diffs/react';
import { app, call, failure, type Range, type Review } from './api';

export interface Draft {
  text: string;
  /** The lines as the reviewer would write them, or `null` for a comment alone. */
  suggestion: string | null;
}

export const EMPTY: Draft = { text: '', suggestion: null };

export function span({ from, to }: Range): string {
  return from === to
    ? `Line ${String(from)}`
    : `Lines ${String(from)}–${String(to)}`;
}

export function useScheme(): 'light' | 'dark' {
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
export function useKeyboardReveal() {
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

/**
 * Telegram's main button sending the `count` comments `review` makes, hidden while the
 * editor is open, where the keyboard would lift it over the editor, and a send would
 * drop the draft. Closing the page asks first while it holds comments.
 */
export function useSending(
  count: number,
  editing: boolean,
  review: () => Review,
) {
  useEffect(() => {
    if (count > 0) {
      app.enableClosingConfirmation();
    } else {
      app.disableClosingConfirmation();
    }
  }, [count]);

  const send = useStableCallback(async () => {
    app.MainButton.showProgress();
    try {
      await call('api/review', review());
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
      text: `Send review (${String(count)})`,
      is_visible: count > 0 && !editing,
    });
  }, [count, editing]);
}

/**
 * The mouse event a browser fires after a tap would move the focus from the editor the
 * tap opened to the line number tapped.
 */
export function keepFocus(event: React.MouseEvent) {
  const number = event.nativeEvent
    .composedPath()
    .some(
      (node) =>
        node instanceof Element && node.hasAttribute('data-column-number'),
    );
  if (number) {
    event.preventDefault();
  }
}

export function Editor({
  label,
  original,
  draft,
  onDraft,
  onSave,
  onCancel,
}: {
  label: string;
  /** The lines a suggestion starts from, or `null` for lines it cannot rewrite. */
  original: string | null;
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
      <div className="label">{label}</div>
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
        {original !== null && (
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
        )}
        <span className="grow" />
        <button onClick={onCancel}>Cancel</button>
        <button className="primary" disabled={empty} onClick={onSave}>
          Save
        </button>
      </div>
    </div>
  );
}

export function NoteView({
  label,
  draft,
  original,
  onEdit,
  onDelete,
}: {
  label: string;
  draft: Draft;
  /** The lines its suggestion rewrites. */
  original: readonly string[];
  /** Opens the note in the editor, or `null` while the editor is open on other lines. */
  onEdit: (() => void) | null;
  onDelete: () => void;
}) {
  return (
    <div className="extension note">
      <div className="label">{label}</div>
      {draft.suggestion !== null && (
        <pre className="suggestion">
          {original.map((line, index) => (
            <div key={`-${String(index)}`} className="removed">
              - {line}
            </div>
          ))}
          {draft.suggestion.split('\n').map((line, index) => (
            <div key={`+${String(index)}`} className="added">
              + {line}
            </div>
          ))}
        </pre>
      )}
      {draft.text && <div className="text">{draft.text}</div>}
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

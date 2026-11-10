import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import {
  getFiletypeFromFileName,
  getSharedHighlighter,
  type DiffsHighlighter,
  type ThemedToken,
} from '@pierre/diffs';
import { useStableCallback } from '@pierre/diffs/react';
import { app, call, failure, type Range, type Review } from './api';

export interface Draft {
  text: string;
  /** The lines as the reviewer would write them, or `null` for a comment alone. */
  suggestion: string | null;
}

export const EMPTY: Draft = { text: '', suggestion: null };

export const THEMES = { dark: 'one-dark-pro', light: 'one-light' } as const;

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
 * A sheet sliding up over the dimmed page, closed by a tap outside it, Telegram's back
 * button or Escape. It renders in the body, since the view it opens from scrolls and
 * transforms.
 */
export function Sheet({
  heading,
  onClose,
  children,
}: {
  heading: string;
  onClose: () => void;
  children: React.ReactNode;
}) {
  const closed = useStableCallback(onClose);
  useEffect(() => {
    const pressed = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        closed();
      }
    };
    app.BackButton.onClick(closed);
    app.BackButton.show();
    window.addEventListener('keydown', pressed);
    return () => {
      app.BackButton.offClick(closed);
      app.BackButton.hide();
      window.removeEventListener('keydown', pressed);
    };
  }, [closed]);
  return createPortal(
    <div className="backdrop" onClick={onClose}>
      <div
        className="sheet"
        role="dialog"
        onClick={(event) => {
          event.stopPropagation();
        }}
      >
        <div className="heading">{heading}</div>
        {children}
      </div>
    </div>,
    document.body,
  );
}

/**
 * Telegram's main button sending the `count` comments `review` makes, with the message
 * its secondary button opens a sheet to write. Both hide while the editor is open, where
 * the keyboard would lift them over the editor, and a send would drop the draft. The main
 * button stays over the message's sheet, so it sends what is written there. Closing the
 * page asks first while it holds comments.
 */
export function Sending({
  count,
  editing,
  review,
}: {
  count: number;
  editing: boolean;
  review: () => Omit<Review, 'text'>;
}) {
  const [text, setText] = useState('');
  const [writing, setWriting] = useState(false);

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
      await call('api/review', { ...review(), text: text.trim() });
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
    const opened = () => {
      setWriting(true);
    };
    app.MainButton.onClick(clicked);
    app.SecondaryButton.onClick(opened);
    return () => {
      app.MainButton.offClick(clicked);
      app.SecondaryButton.offClick(opened);
    };
  }, [send]);

  const shown = count > 0 && !editing;
  const written = text.trim() !== '';
  useEffect(() => {
    app.MainButton.setParams({
      text: `Send review (${String(count)})`,
      is_visible: shown,
    });
    app.SecondaryButton.setParams({
      text: written ? 'Edit message' : 'Add message',
      position: 'left',
      is_visible: shown && !writing,
    });
  }, [count, shown, written, writing]);

  return (
    writing && (
      <Sheet
        heading="Message"
        onClose={() => {
          setWriting(false);
        }}
      >
        <div className="editor message">
          <textarea
            autoFocus
            rows={4}
            value={text}
            placeholder="A message to send with the comments"
            onChange={(event) => {
              setText(event.target.value);
            }}
          />
        </div>
      </Sheet>
    )
  );
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

type Token = Pick<ThemedToken, 'content' | 'offset' | 'color'>;

/**
 * The lines of `code` split into tokens in the colors the code view gives `path`'s
 * language, or uncolored until the highlighter loads.
 */
function useTokens(path: string, code: string): readonly (readonly Token[])[] {
  const scheme = useScheme();
  const lang = getFiletypeFromFileName(path);
  const [highlighter, setHighlighter] = useState<DiffsHighlighter | null>(null);
  useEffect(() => {
    void getSharedHighlighter({
      themes: Object.values(THEMES),
      langs: [lang],
    }).then(setHighlighter);
  }, [lang]);
  return useMemo(
    () =>
      highlighter?.codeToTokens(code, { lang, theme: THEMES[scheme] }).tokens ??
      code.split('\n').map((content) => [{ content, offset: 0 }]),
    [highlighter, lang, scheme, code],
  );
}

const colored = (tokens: readonly Token[]) =>
  tokens.map(({ content, offset, color }) => (
    <span key={offset} style={{ color }}>
      {content}
    </span>
  ));

/**
 * A field for code, drawn in the colors the code view gives `path`'s language: the
 * text typed is transparent over a copy drawn in those colors, laid out the same way.
 */
function Code({
  path,
  value,
  onChange,
}: {
  path: string;
  value: string;
  onChange: (value: string) => void;
}) {
  const lines = useTokens(path, value);
  return (
    <div className="code">
      <pre aria-hidden>
        {lines.map((tokens, line) => (
          <span key={line}>
            {line > 0 && '\n'}
            {colored(tokens)}
          </span>
        ))}
        {/* A text ending in a line break shows one more line in the field. */}{' '}
      </pre>
      <textarea
        value={value}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        onChange={(event) => {
          onChange(event.target.value);
        }}
      />
    </div>
  );
}

export function Editor({
  label,
  path,
  original,
  draft,
  onDraft,
  onSave,
  onCancel,
}: {
  label: string;
  path: string;
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
        <Code
          path={path}
          value={suggestion}
          onChange={(value) => {
            onDraft({ text, suggestion: value });
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

function Suggestion({
  path,
  original,
  suggestion,
}: {
  path: string;
  original: string;
  suggestion: string;
}) {
  const removed = useTokens(path, original);
  const added = useTokens(path, suggestion);
  return (
    <pre className="suggestion">
      {removed.map((tokens, index) => (
        <div key={`-${String(index)}`} className="removed">
          - {colored(tokens)}
        </div>
      ))}
      {added.map((tokens, index) => (
        <div key={`+${String(index)}`} className="added">
          + {colored(tokens)}
        </div>
      ))}
    </pre>
  );
}

export interface Entry {
  key: string;
  label: string;
  draft: Draft;
  onJump: () => void;
}

/** The first line of a comment's text, or `Suggested change` for a suggestion without text. */
function headline({ text }: Draft): string {
  const [first = ''] = text.trim().split('\n');
  return first === '' ? 'Suggested change' : first;
}

/** The comments saved, each scrolling the page to its lines on a tap. */
export function Overview({ entries }: { entries: readonly Entry[] }) {
  return (
    entries.length > 0 && (
      <section className="section">
        <div className="label">Comments</div>
        {entries.map(({ key, label, draft, onJump }) => (
          <button key={key} className="row" onClick={onJump}>
            <span className="hint">{label}</span>
            <span>{headline(draft)}</span>
          </button>
        ))}
      </section>
    )
  );
}

export function NoteView({
  label,
  path,
  draft,
  original,
  onEdit,
  onDelete,
}: {
  label: string;
  path: string;
  draft: Draft;
  /** The lines a suggestion starts from, or `null` for lines it cannot rewrite. */
  original: string | null;
  /** Opens the note in the editor, or `null` while the editor is open on other lines. */
  onEdit: (() => void) | null;
  onDelete: () => void;
}) {
  return (
    <div className="extension note">
      <div className="label">{label}</div>
      {draft.suggestion !== null && original !== null && (
        <Suggestion
          path={path}
          original={original}
          suggestion={draft.suggestion}
        />
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

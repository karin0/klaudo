export const app = window.Telegram.WebApp;

export const failure = (error: unknown) =>
  error instanceof Error ? error.message : String(error);

/** A request to the daemon, which reads who asks and what they may see from `initData`. */
export async function call(path: string, body?: unknown): Promise<Response> {
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

/** Lines of a file, from 1. */
export interface Range {
  from: number;
  to: number;
}

/**
 * What `POST /api/review` takes: the comments, the commit removed lines belong to, and a
 * message for the whole review, empty for none.
 */
export interface Review {
  since: string | null;
  notes: {
    new: (Range & { path: string }) | null;
    old: (Range & { path: string }) | null;
    /** The lines as a unified diff shows them, each behind ` `, `-` or `+`. */
    lines: string[];
    text: string;
    suggestion: string | null;
  }[];
  text: string;
}

export interface Commit {
  hash: string;
  parent: string | null;
  /** In Unix seconds. */
  time: number;
  subject: string;
  included: boolean;
}

/** The commit the changes start from, and the commits the page moves it along. */
export interface Start {
  since: string;
  commits: Commit[];
}

/**
 * A file a message links to, with the lines its links name, and its text or why it cannot
 * be read. `old` is its text as of the start when it changed since, empty when the start
 * had none.
 */
export type Linked = { path: string; lines: Range[] } & (
  { content: string; old?: string } | { refusal: string }
);

/**
 * What `GET /api/open` answers: a session's working tree since a commit, either the files
 * a message links to, with the link's own file and lines among them, or every changed
 * file, and how many unchanged lines the page shows around a change or a linked line.
 */
export type Opened = { context: number } & (
  | {
      kind: 'files';
      /** `null` without a working tree the visitor may see all of. */
      start: Start | null;
      files: Linked[];
      file: number;
      lines: Range | null;
    }
  | {
      kind: 'changes';
      start: Start;
      /** A git patch, untracked files as new ones. */
      patch: string;
    }
);

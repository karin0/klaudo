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

/** A file a message links to, with the lines its links name, and its text or why it cannot be read. */
export type Linked = { path: string; lines: Range[] } & (
  { content: string } | { refusal: string }
);

/**
 * What `GET /api/open` answers: the files a message links to, with the link's own file
 * among them and its lines, or the changes a link opens.
 */
export type Opened =
  | {
      kind: 'files';
      files: Linked[];
      file: number;
      lines: Range | null;
    }
  | {
      kind: 'changes';
      since: string;
      commits: Commit[];
      /** A git patch, untracked files as new ones. */
      patch: string;
    };

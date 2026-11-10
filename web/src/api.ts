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

/** What `POST /api/review` takes: the comments, and the commit removed lines belong to. */
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
}

export interface Commit {
  hash: string;
  parent: string | null;
  /** In Unix seconds. */
  time: number;
  subject: string;
  included: boolean;
}

/** What `GET /api/open` answers: the file a link names, or the changes it opens. */
export type Opened =
  | {
      kind: 'file';
      path: string;
      content: string;
      /** The lines the link points at, 0 for none. */
      from: number;
      to: number;
    }
  | {
      kind: 'changes';
      since: string;
      commits: Commit[];
      /** A git patch, untracked files as new ones. */
      patch: string;
    };

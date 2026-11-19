import { StrictMode, useCallback, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { app, call, failure, type Opened, type Steps } from './api';
import { Review, type Stepped } from './review';
import './app.css';

app.ready();
app.expand();

const get = async <T,>(path: string, query: Record<string, string>) => {
  const response = await call(
    `${path}?${new URLSearchParams(query).toString()}`,
  );
  return (await response.json()) as T;
};

/** What the link opens since `since`, or its own start, commit by commit when `stepped`. */
async function load(
  since: string | undefined,
  stepped: boolean,
): Promise<{ opened: Opened; stepped: Stepped | null }> {
  const opened = await get<Opened>(
    'api/open',
    since === undefined ? {} : { since },
  );
  const head = opened.start?.commits[0]?.hash;
  if (!stepped || opened.start === null || head === undefined) {
    return { opened, stepped: null };
  }
  // The working tree's own changes are those since `HEAD`.
  const [steps, tree] = await Promise.all([
    get<Steps>('api/steps', { since: opened.start.since }),
    get<Opened>('api/open', { since: head }),
  ]);
  return { opened, stepped: { steps, tree } };
}

function App() {
  const [shown, setShown] = useState<Awaited<ReturnType<typeof load>> | null>(
    null,
  );
  const [refusal, setRefusal] = useState<string | null>(null);
  // The page moves its start with `since`, and stays as it was on a refusal.
  const open = useCallback((since?: string, stepped = false) => {
    load(since, stepped).then(setShown, (error: unknown) => {
      if (since === undefined) {
        setRefusal(failure(error));
      } else {
        app.showAlert(failure(error));
      }
    });
  }, []);
  useEffect(() => {
    open();
  }, [open]);
  if (shown === null) {
    return <p className="hint status">{refusal ?? 'Loading…'}</p>;
  }
  const { opened, stepped } = shown;
  return (
    <Review
      key={`${opened.start?.since ?? ''} ${String(stepped !== null)}`}
      opened={opened}
      stepped={stepped}
      onMove={open}
    />
  );
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

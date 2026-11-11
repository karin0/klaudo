import { StrictMode, useCallback, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { app, call, failure, type Opened } from './api';
import { ChangesReview } from './changes';
import { FilesReview } from './file';
import './app.css';

app.ready();
app.expand();

function App() {
  const [opened, setOpened] = useState<Opened | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);
  // The changes page moves its start with `since`, and stays as it was on a refusal.
  const open = useCallback((since?: string) => {
    const path =
      since === undefined
        ? 'api/open'
        : `api/open?${new URLSearchParams({ since }).toString()}`;
    call(path)
      .then(async (response) => (await response.json()) as Opened)
      .then(setOpened, (error: unknown) => {
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
  if (opened === null) {
    return <p className="hint status">{refusal ?? 'Loading…'}</p>;
  }
  return opened.kind === 'files' ? (
    <FilesReview shown={opened} />
  ) : (
    <ChangesReview key={opened.since} changes={opened} onMove={open} />
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

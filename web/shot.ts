/**
 * Draws the page as the dev server's mock opens it in a headless Chromium and saves a
 * screenshot, as in `CHROME=<binary> pnpm shot '/mock?changes' changes.png`. A third
 * argument is JavaScript the page evaluates before the screenshot, to tap what a visitor
 * would. The run fails when the page throws or logs an error, as the library does while
 * drawing a diff it takes for inconsistent, which the unit tests never draw.
 */
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
import { setTimeout as sleep } from 'node:timers/promises';
import { createServer } from 'vite';

/** How long the browser may take to start or to answer. */
const WAIT = 15_000;
/** How long the page is given to load what it shows, after it opens and after `script`. */
const SETTLE = 3000;

interface Message {
  id?: number;
  method?: string;
  params?: {
    exceptionDetails?: { text: string; exception?: { description?: string } };
    type?: string;
    args?: { value?: unknown; description?: string }[];
  };
  result?: { data?: string; result?: { value?: unknown } };
}

const [path, file, script] = process.argv.slice(2);
const chrome = process.env.CHROME;
if (path === undefined || file === undefined || chrome === undefined) {
  throw new Error('usage: CHROME=<binary> pnpm shot <path> <file> [script]');
}

const late = async (what: string): Promise<never> => {
  await sleep(WAIT);
  throw new Error(`${what} took too long`);
};

const server = await createServer();
await server.listen();
const [origin] = server.resolvedUrls?.local ?? [];
// Without a page to open, the browser offers the debugger none to draw in.
const browser = spawn(
  chrome,
  ['--headless', '--remote-debugging-port=0', 'about:blank'],
  { stdio: ['ignore', 'ignore', 'pipe'] },
);
let thrown: string[] = [];
try {
  // The browser prints where its debugger listens once it has picked a port.
  const listening = (async () => {
    for await (const line of createInterface(browser.stderr)) {
      const port = /^DevTools listening on ws:\/\/[^:]+:(\d+)\//u.exec(line);
      if (port?.[1] !== undefined) {
        return port[1];
      }
    }
    throw new Error(`${chrome} exited without a debugger`);
  })();
  const port = await Promise.race([listening, late('the browser')]);
  const targets = (await (
    await fetch(`http://127.0.0.1:${port}/json`)
  ).json()) as { type: string; webSocketDebuggerUrl: string }[];
  const target = targets.find(({ type }) => type === 'page');
  if (target === undefined) {
    throw new Error('the browser opened no page');
  }
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await Promise.race([
    new Promise((resolve) => {
      socket.addEventListener('open', resolve);
    }),
    late('the debugger'),
  ]);
  const answers = new Map<number, (message: Message) => void>();
  socket.addEventListener('message', ({ data }) => {
    const message = JSON.parse(String(data)) as Message;
    const { exceptionDetails: details, type, args = [] } = message.params ?? {};
    if (message.id !== undefined) {
      answers.get(message.id)?.(message);
    } else if (details !== undefined) {
      thrown = [...thrown, details.exception?.description ?? details.text];
    } else if (
      message.method === 'Runtime.consoleAPICalled' &&
      type === 'error'
    ) {
      const logged = args.map((arg) => arg.description ?? String(arg.value));
      thrown = [...thrown, logged.join(' ')];
    }
  });
  let sent = 0;
  const send = (method: string, params: object = {}) => {
    sent += 1;
    const id = sent;
    const answered = new Promise<Message>((resolve) => {
      answers.set(id, resolve);
    });
    socket.send(JSON.stringify({ id, method, params }));
    return Promise.race([answered, late(method)]);
  };

  await send('Runtime.enable');
  await send('Page.enable');
  await send('Emulation.setDeviceMetricsOverride', {
    width: 420,
    height: 1400,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await send('Page.navigate', { url: new URL(path, origin).href });
  await sleep(SETTLE);
  if (script !== undefined) {
    const evaluated = await send('Runtime.evaluate', {
      expression: script,
      awaitPromise: true,
    });
    console.log(evaluated.result?.result?.value);
    await sleep(SETTLE);
  }
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  writeFileSync(file, Buffer.from(shot.result?.data ?? '', 'base64'));
} finally {
  browser.kill();
  await server.close();
}
if (thrown.length > 0) {
  throw new Error(`the page threw or logged an error:\n${thrown.join('\n')}`);
}

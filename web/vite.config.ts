import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import type { ServerResponse } from 'node:http';
import { defineConfig, type Plugin } from 'vite';
import react from '@vitejs/plugin-react';

const THEMES = {
  light: {
    bg_color: '#ffffff',
    text_color: '#000000',
    hint_color: '#707579',
    secondary_bg_color: '#f4f4f5',
    section_separator_color: '#e7e7e7',
    button_color: '#3390ec',
    button_text_color: '#ffffff',
    link_color: '#3390ec',
    destructive_text_color: '#e53935',
    section_header_text_color: '#3390ec',
  },
  dark: {
    bg_color: '#212121',
    text_color: '#ffffff',
    hint_color: '#aaaaaa',
    secondary_bg_color: '#181818',
    section_separator_color: '#303030',
    button_color: '#8774e1',
    button_text_color: '#ffffff',
    link_color: '#8774e1',
    destructive_text_color: '#ff595a',
    section_header_text_color: '#8774e1',
  },
};

/** Runs git in the daemon's own repository. */
const git = (...args: string[]) =>
  execFileSync(
    'git',
    ['-C', new URL('..', import.meta.url).pathname, ...args],
    {
      encoding: 'utf8',
      maxBuffer: 1 << 26,
    },
  );

/**
 * The daemon's API and Telegram's launch parameters on the dev server, so the page runs
 * in a browser without either. `/mock` opens the page as Telegram would on a file link,
 * `/mock?changes` on the link to a turn's changes, and `dark` in either query the dark
 * theme. Both show the daemon's own working tree since three commits back, untracked
 * files left out. The files are some of the daemon's own and one refused, opening at the
 * second link to `src/telegram.rs`, and their lines count as lines of `HEAD` in the
 * changes commit by commit. A review sent is printed by the dev server. A
 * browser draws no `MainButton`, so `Telegram.WebView.receiveEvent('main_button_pressed')`
 * in the console presses it.
 */
/** The files the mocked file link names, with the lines it names in each. */
const LINKED: [string, [number, number][]][] = [
  ['src/web.rs', [[13, 14]]],
  [
    'src/telegram.rs',
    [
      [22, 24],
      [600, 610],
    ],
  ],
  ['README.md', []],
];

function mock(): Plugin {
  return {
    name: 'klaudo-mock',
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use('/mock', (request, response) => {
        const theme = request.url?.includes('dark') ? 'dark' : 'light';
        const start = request.url?.includes('changes') ? 'changes' : 'file';
        const launch = new URLSearchParams({
          tgWebAppData: new URLSearchParams({
            user: JSON.stringify({ id: 1, first_name: 'Mock' }),
            auth_date: '0',
            start_param: start,
            hash: '0',
          }).toString(),
          tgWebAppThemeParams: JSON.stringify(THEMES[theme]),
          tgWebAppVersion: '9.1',
          tgWebAppPlatform: 'weba',
        });
        response.statusCode = 302;
        response.setHeader('Location', `/#${launch.toString()}`);
        response.end();
      });
      const answer = (response: ServerResponse, body: unknown) => {
        response.setHeader('Content-Type', 'application/json');
        response.end(JSON.stringify(body));
      };
      const read = (path: string) =>
        readFileSync(new URL(`../${path}`, import.meta.url), {
          encoding: 'utf8',
        });
      server.middlewares.use('/api/open', (request, response) => {
        const data = request.headers.authorization?.replace(/^tma /, '') ?? '';
        const query = new URLSearchParams(request.url?.split('?')[1]);
        const since = query.get('since') ?? git('rev-parse', 'HEAD~3').trim();
        const included = git('rev-list', '--first-parent', `${since}..HEAD`);
        const log = git(
          'log',
          '--first-parent',
          '--max-count=30',
          '--date=unix',
          '--format=%H%x00%P%x00%cd%x00%s',
        );
        const start = {
          since,
          commits: log
            .trim()
            .split('\n')
            .map((line) => {
              const [hash = '', parents = '', time = '', subject = ''] =
                line.split('\0');
              const [parent = null] = parents.split(' ').filter(Boolean);
              return {
                hash,
                parent,
                time: Number(time),
                subject,
                included: included.includes(hash),
              };
            }),
        };
        if (new URLSearchParams(data).get('start_param') === 'file') {
          const linked = (path: string, lines: [number, number][]) => {
            const content = read(path);
            const old = git('show', `${since}:${path}`);
            return {
              path,
              lines: lines.map(([from, to]) => ({ from, to })),
              content,
              ...(old !== content && { old }),
            };
          };
          answer(response, {
            context: 5,
            kind: 'files',
            start,
            files: [
              ...LINKED.map(([path, lines]) => linked(path, lines)),
              { path: '../outside', lines: [], refusal: 'no such file' },
            ],
            file: 1,
            lines: { from: 600, to: 610 },
          });
          return;
        }
        answer(response, {
          context: 5,
          kind: 'changes',
          start,
          patch: git(
            'diff-index',
            '--patch',
            '--no-color',
            '-M',
            '--unified=5',
            '--src-prefix=a/',
            '--dst-prefix=b/',
            since,
          ),
        });
      });
      server.middlewares.use('/api/steps', (request, response) => {
        const data = request.headers.authorization?.replace(/^tma /, '') ?? '';
        const query = new URLSearchParams(request.url?.split('?')[1]);
        const followed = LINKED.flatMap(([path, lines]) =>
          (lines.length > 0
            ? lines
            : [[1, git('show', `HEAD:${path}`).split('\n').length - 1]]
          ).map(([from, to]) => `-L${String(from)},${String(to)}:${path}`),
        );
        const log = git(
          'log',
          '--first-parent',
          '--max-count=30',
          '--date=unix',
          '--format=%x00%H%x00%P%x00%cd%x00%s%x00',
          '--src-prefix=a/',
          '--dst-prefix=b/',
          ...(new URLSearchParams(data).get('start_param') === 'file'
            ? followed
            : ['--patch', '-M', '--unified=5']),
          `${query.get('since') ?? ''}..HEAD`,
        );
        const fields = log.split('\0').slice(1);
        const steps = [];
        for (let at = 0; at + 4 < fields.length; at += 5) {
          const [hash, parents = '', time, subject, patch = ''] = fields.slice(
            at,
            at + 5,
          );
          const [parent = null] = parents.split(' ').filter(Boolean);
          steps.unshift({
            commit: {
              hash,
              parent,
              time: Number(time),
              subject,
              included: true,
            },
            patch: patch.replace(/^\n+/, ''),
          });
        }
        answer(response, { steps, more: false });
      });
      server.middlewares.use('/api/contents', (request, response) => {
        const query = new URLSearchParams(request.url?.split('?')[1]);
        const [since, path, prev] = ['since', 'path', 'prev'].map(
          (name) => query.get(name) ?? '',
        );
        const at = query.get('at');
        answer(response, {
          old: git('show', `${since ?? ''}:${prev ?? ''}`),
          new:
            at === null ? read(path ?? '') : git('show', `${at}:${path ?? ''}`),
        });
      });
      server.middlewares.use('/api/review', (request, response) => {
        let body = '';
        request.on('data', (chunk: Buffer) => {
          body += chunk.toString();
        });
        request.on('end', () => {
          server.config.logger.info(`review: ${body}`);
          response.statusCode = 204;
          response.end();
        });
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), mock()],
  // The daemon may be reached under any path prefix.
  base: './',
  build: {
    // Shiki's grammars of the largest languages pass Vite's default limit, and the page
    // loads each only for a file in its language.
    chunkSizeWarningLimit: 1024,
  },
});

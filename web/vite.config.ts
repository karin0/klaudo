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
 * theme. The file shown is the daemon's own `src/telegram.rs`, and the changes are the
 * daemon's own since three commits back, untracked files left out. A review sent is
 * printed by the dev server. A browser draws no `MainButton`, so
 * `Telegram.WebView.receiveEvent('main_button_pressed')` in the console presses it.
 */
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
        if (new URLSearchParams(data).get('start_param') === 'file') {
          const path = 'src/telegram.rs';
          answer(response, {
            kind: 'file',
            path,
            content: read(path),
            from: 22,
            to: 24,
          });
          return;
        }
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
        answer(response, {
          kind: 'changes',
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
          patch: git(
            'diff-index',
            '--patch',
            '--no-color',
            '-M',
            '--src-prefix=a/',
            '--dst-prefix=b/',
            since,
          ),
        });
      });
      server.middlewares.use('/api/contents', (request, response) => {
        const query = new URLSearchParams(request.url?.split('?')[1]);
        const [since, path, prev] = ['since', 'path', 'prev'].map(
          (name) => query.get(name) ?? '',
        );
        answer(response, {
          old: git('show', `${since ?? ''}:${prev ?? ''}`),
          new: read(path ?? ''),
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

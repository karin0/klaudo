import { readFileSync } from 'node:fs';
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

/**
 * The daemon's API and Telegram's launch parameters on the dev server, so the page runs
 * in a browser without either. `/mock` opens the page as Telegram would, and
 * `/mock?dark` in the dark theme. The file shown is the daemon's own `src/telegram.rs`,
 * and a review sent is printed by the dev server. A browser draws no `MainButton`, so
 * `Telegram.WebView.receiveEvent('main_button_pressed')` in the console presses it.
 */
function mock(): Plugin {
  return {
    name: 'klaudo-mock',
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use('/mock', (request, response) => {
        const theme = request.url?.includes('dark') ? 'dark' : 'light';
        const launch = new URLSearchParams({
          tgWebAppData: new URLSearchParams({
            user: JSON.stringify({ id: 1, first_name: 'Mock' }),
            auth_date: '0',
            start_param: 'mock',
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
      server.middlewares.use('/api/file', (_request, response) => {
        const path = 'src/telegram.rs';
        const content = readFileSync(new URL(`../${path}`, import.meta.url), {
          encoding: 'utf8',
        });
        response.setHeader('Content-Type', 'application/json');
        response.end(JSON.stringify({ path, content, from: 22, to: 24 }));
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

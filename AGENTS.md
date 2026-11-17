# Klaŭdo

The documents under `docs/` record the behavior and design of the code each one covers:

- [docs/architecture.md](docs/architecture.md): the hook, the daemon and the status line,
  in `src/main.rs`, `src/hook.rs`, `src/listen.rs` and `src/listen/usage.rs`.
- [docs/turns.md](docs/turns.md): how a turn and the files a session sends are posted, in
  `src/telegram.rs`, `src/main.rs` and `src/listen/turn.rs`, `render.rs`,
  `transcript.rs`, `usage.rs` and `price.rs`.
- [docs/chat.md](docs/chat.md): messages from the chat and the terminals they are typed
  into, in `src/tmux.rs` and `src/listen/chat.rs`, `attach.rs`, `issues.rs` and
  `program.rs`.
- [docs/topics.md](docs/topics.md): the topic a session stands in, `/resume` and
  `/rewind`, in `src/listen.rs` and `src/listen/chat.rs` and `rewind.rs`.
- [docs/review.md](docs/review.md): the review page, in `src/web.rs`, `src/changes.rs`,
  `src/listen/review.rs`, `src/listen/diff.rs` and `web/`.

## Checks

`./check.sh` runs every check and the release build. `pnpm -C web dev` serves the
review page in a browser, as `web/vite.config.ts` describes.

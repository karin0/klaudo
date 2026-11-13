# Hook and daemon

Claude Code runs `exec klaudo` as the hook of every event, and one daemon per machine,
`klaudo listen`, makes every Telegram call.

The terminal draws streamed text only once the `MessageDisplay` hook returns, so the
hook cannot touch the network. That is why the hook is a compiled binary, since a shell
script costs 9.4 ms per invocation against 0.5 ms for the same handoff. The hook writes
the event to the stream socket `$XDG_RUNTIME_DIR/klaudo/listen.sock` and exits. The
socket is a stream since `PostToolUse` carries the tool's whole output, which can pass
the size limit of a datagram.

The daemon exists since the open segment's clock has to move while nothing happens, the
answer must not race a rewrite still in flight, and Telegram hands updates to one reader
per bot, who has to answer when no turn is running. Routing a reply needs
to know which session a message belongs to, so the reader is the process that posts.

The daemon writes the pane of each session to `$XDG_RUNTIME_DIR/klaudo/state.json` as it changes, so an
idle session stays reachable across a restart, and the directory of each exited one to
`$XDG_STATE_HOME/klaudo/ended.json`, so a reply resumes it after a reboot too. The
daemon keeps that record itself, since the transcripts under `~/.claude/projects` are in
a format Claude Code has not published. A turn in flight is lost.

## Status line

The status line is the one place Claude Code reports the context window and the plan's
limits, so `klaudo status` forwards what it receives to the daemon and prints nothing. A
session of the Agent SDK or the VS Code extension draws no status line, so a program
that learns the limits some other way, such as from the SDK's rate limit events, can
pipe them to `klaudo status` itself. Only `session_id` is required, and the limits count
for every session of the machine, whichever session reports them:

```json
{"session_id": "…", "rate_limits": {
  "five_hour": {"used_percentage": 12.5, "resets_at": 1789071000},
  "seven_day": {"used_percentage": 56, "resets_at": 1789500000}}}
```

`used_percentage` runs from 0 to 100 and `resets_at` is in Unix seconds. Either window
may be left out, and the daemon ignores every other field. Without a daemon,
`klaudo status` writes `klaudo: no daemon is listening` to stderr and still exits with 0.

## Hooks without a daemon

With no daemon listening, `UserPromptSubmit`, `Stop`, `StopFailure`, `Notification` and
`PostCompact` send from the hook process itself, each as a message of its own, and
nothing can be sent back. Every other event is dropped, since posted alone it would be
one call per tool call or streamed fragment, and `PreToolUse` holds up its call until
the hook returns. Such an answer keeps its links as Claude Code wrote them.

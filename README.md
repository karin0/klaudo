# klaude

Pushes a Claude Code session's replies to a Telegram private chat, so a long turn can be
left alone and picked up when it is worth returning to.

## What lands in the chat

A turn produces one or more response segments, each an assistant message with text in
it. A segment streams into a Telegram draft while it is being written, and becomes a
message once it is complete, which is when the next segment starts. While the draft is
on screen it carries a status line under the text, a word from Claude Code's own
vocabulary and the turn's elapsed time, stepping to the next word on every refresh. The
last segment is not posted on its own: the `Stop` event carries its text, so posting it
would put the same words in the chat twice.

So a turn that talked twice around a tool call leaves both halves in the chat, in order,
and the last message is the only one that makes a sound. Each carries the elapsed time
it was posted at; only the last carries the `#claude` tag, which therefore counts turns
rather than segments.

Every message opens with the same line: the project directory, then
`session/prompt` shortened to eight characters each. The prompt half comes from the
`prompt_id` that Claude Code keeps constant from one user prompt until the next, so the
messages of one turn share it and the next turn reads differently.

## Hooks

One command answers every event, so `settings.json` repeats it under `UserPromptSubmit`,
`MessageDisplay`, `Stop`, `StopFailure` and `Notification`:

```json
{"type": "command", "command": "set -a && . <secrets file> && exec <path to klaude>"}
```

`UserPromptSubmit` starts the daemon and sends nothing, which is also where the turn's
clock starts. Every other event is reported.

## Configuration

`BOT_TOKEN` and `CHAT_ID` are read from the environment. That is what the first half of
the hook command is for: `set -a` marks what follows for export, so a file of plain
shell assignments becomes variables the binary after it can see. `CHAT_ID` is the
integer id of a private chat, because Telegram accepts a draft only there.

`API_BASE` is optional and defaults to `https://api.telegram.org`; the test points it at
a server of its own.

## Why a daemon owns the turn

The `MessageDisplay` hook runs on every flush of streamed text and the terminal draws
that text only once the hook returns, so nothing that touches the network can happen in
the hook process. It writes the event to a unix datagram socket and exits.

Three constraints then land on the process at the other end. A draft disappears 30
seconds after its last frame, so a turn that goes quiet inside a long tool call needs
frames anyway. The final message must not race a frame still in flight, which is free
once one process issues every call of the turn in order. And the elapsed time in the
final message is just the age of the daemon, which is why nothing here reads the
transcript to find out when the turn began.

The daemon lives for exactly one turn. It starts at `UserPromptSubmit` and exits after
`Stop`, so its state needs no expiry rules and no cleanup pass.

## When the daemon is not there

A resumed session, or a daemon that died, leaves the socket unanswered. `Stop`,
`StopFailure` and `Notification` then send from the hook process itself, so the chat
still gets the turn; the draft is what goes missing.

## Checks

`./check.sh` runs shellcheck, formatting, clippy, the tests and a release build. The
lint levels live in `Cargo.toml`, so a bare `cargo clippy` and whatever an editor runs
in the background enforce the same set rather than only this script.

`tests/turn.rs` drives the hook chain end to end in a throwaway runtime directory,
against a server of its own that answers the way Telegram does. It asserts the calls a
two-segment turn makes, their order, and which of them carries a notification.

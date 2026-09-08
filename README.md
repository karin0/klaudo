# klaude

Carries a Claude Code session's turns to a Telegram private chat and carries what you
type there back into the session's terminal, so a long turn can be left alone, picked up
from the phone, and answered from there.

## What lands in the chat

A turn opens with the prompt, quoted and without a sound, so a chat scrolled through
tells the asks from the answers by their shape alone. Every message
the turn sends afterwards is a Telegram reply to that one, so a chat that collects many
turns reads as a thread per turn. A prompt klaude typed is already in the chat as the
message that asked for it, and its turn threads under that message. A prompt deleted
from the chat leaves the rest of its turn arriving as messages of their own.

A turn produces one or more response segments, each an assistant message with text in
it. A segment streams into a Telegram draft while it is being written, and becomes a
message once it is complete, which is when the next segment starts. While the draft is
on screen it carries a status line under the text, a word from Claude Code's own
vocabulary and the turn's elapsed time, stepping to the next word on every refresh. A
turn opens its draft when it starts, with the status line and no text above it, so the
minutes it spends thinking or in tool calls are on screen as they pass. The
last segment is not posted on its own: the `Stop` event carries its text, so posting it
would put the same words in the chat twice.

So a turn that talked twice around a tool call leaves both halves in the chat, in order,
and the last message is the only one that makes a sound. Each carries the elapsed time
it was posted at; only the last carries the `#claude` tag, which therefore counts turns
rather than segments.

Every message opens with the same line: the directory the session was opened in, then
`session/prompt` shortened to eight characters each. A turn that runs `cd` reports the
directory it moved to, and the line still names the one Claude Code files the session's
transcript under. That line is also the address a reply is routed by.

## What you can send

What a message replies to is where it goes. A message replying to nothing goes to the
session klaude heard from last, and the turn it starts threads under it, whose head
names the session that took it.

Replying to any message from a turn types the text into that session's terminal, as a
prompt in its input box. Multiple lines arrive as multiple lines, and quotes, backticks
and non-ASCII text need no escaping, because the text travels through a tmux paste
buffer rather than a shell argument. Sending while a turn is running leaves the prompt
queued, which is what the terminal does with anything typed then.

A message whose text reached an input box gets a 👀 reaction once Claude Code reports
the prompt, so a chat scrolled back shows which asks were accepted.

`/new <directory>` posts an anchor naming that directory and starts nothing. Replying to
the anchor opens a window running `claude` there and types the reply as its first
prompt. Those windows live in a tmux session called `klaude`, one window per
conversation, so `tmux attach -t klaude` reaches a conversation that began on the phone.

A session that opens a directory for the first time stops at the dialog asking whether
the folder is trusted, and reports what it is showing to the chat rather than typing
into a dialog. Answer that once locally and the directory stays trusted.

Only the chat's own owner is answered: a message is acted on when both the chat and the
sender are `CHAT_ID`.

## Why keystrokes

Claude Code's own local messaging socket delivers text to a running session too, and
what arrives there is labelled as coming from another Claude session, carrying the
instruction to treat it as a peer's request and never as the user's approval. That is a
deliberate guardrail against permission laundering, so klaude does not go through it.
`send-keys` reaches the input box, which is the path a person's own typing takes, so the
prompt is the user's because the keystrokes are.

The pane a session lives in is checked before every delivery: `/proc/<pid>/stat` names
the terminal the session process is on, and it has to be the one tmux reports for that
pane. A session that exited leaves its pane to a shell, where the same text would run as
a command.

A session running outside tmux has no pane to type into, and a reply aimed at one is
answered in the chat saying so.

## Hooks

One command answers every event, so `settings.json` repeats it under `SessionStart`,
`UserPromptSubmit`, `MessageDisplay`, `Stop`, `StopFailure` and `Notification`:

```json
{"type": "command", "command": "set -a && . <secrets file> && exec <path to klaude>"}
```

The hook writes the event to a unix datagram socket and exits. Along with the event it
carries `$TMUX`, `$TMUX_PANE` and its own parent process id, which is where the resident
learns which terminal a session is on. The parent is the session because of `exec`:
without it the parent would be the shell that read the secrets file, which exits
immediately.

`SessionStart` fires once the session is ready for input, after the trust dialog, so it
is both how a session announces where it lives and how a conversation opened from the
chat knows when to type its first prompt.

## Configuration

`BOT_TOKEN` and `CHAT_ID` are read from the environment. That is what the first half of
the hook command is for: `set -a` marks what follows for export, so a file of plain
shell assignments becomes variables the binary after it can see. `CHAT_ID` is the
integer id of a private chat, because Telegram accepts a draft only there, and it is the
sender every incoming message is checked against.

`API_BASE` is optional and defaults to `https://api.telegram.org`; the test points it at
a server of its own.

`klaude.service` runs the resident. It reads the same assignments from
`~/.config/klaude/env`, so point that at the file the hook command sources:

```sh
mkdir -p ~/.config/klaude && ln -s <secrets file> ~/.config/klaude/env
systemctl --user enable --now klaude
```

## Why one resident owns everything

The `MessageDisplay` hook runs on every flush of streamed text and the terminal draws
that text only once the hook returns, so nothing that touches the network can happen in
the hook process. That budget is why the hook is a compiled binary: on the machine
klaude was written for, the shell scripts it replaced cost 9.4 ms per invocation,
against 0.5 ms for the same handoff.

The process at the other end runs for as long as the machine does, one per machine.
Three constraints put it there. A draft disappears 30 seconds after its last frame, so a
turn that goes quiet inside a long tool call needs frames anyway. The final message must
not race a frame still in flight, and the replies all carry the id Telegram gave the
prompt message; both are free once one process issues every call in order.

The third constraint is the chat. Telegram hands updates to one reader per bot, and a
message from the phone has to be answerable when no turn is running, which is exactly
when a per-turn process would not exist. So the reader is machine-wide and permanent,
and it is the same process that posts, because routing a reply needs to know which
session a message belongs to. A second process holding that would be a second copy of
the first one's state.

Which session a reply belongs to is read back out of the message being replied to. A
reply carries that message as Telegram rendered it, a list of paragraphs made of spans,
so the address is the code span of its first paragraph. A restarted resident therefore
still routes replies to messages it never posted.

A call Telegram rejects with a rate limit or a failure of its own is asked again up to
three times, waiting the time Telegram names or a doubling one, so a message can arrive
late or, when the answer to an attempt was lost, twice.

The socket has a thread of its own, which moves each datagram into memory as it lands.
A Telegram call holds the machine for as long as the call takes, and the socket's buffer
is a few hundred deltas deep, past which the hooks fall back to posting for themselves.

Per-session state expires on its own. A session is forgotten when `/proc/<pid>` is gone,
and what a session killed mid-turn had already said is posted then.

## Queued prompts

Claude Code fires `UserPromptSubmit` when a prompt is submitted, including one submitted
while a turn is running, and reports that one under the running turn's `prompt_id`. The
queued turn's own id first appears on its own events, once it begins.

So a prompt submitted with a turn open is posted to the chat and its message is queued;
a turn opens when an event names an id that is not the open turn's, and takes the oldest
queued message as the one it replies to. A prompt submitted with nothing running starts
its turn at once, which is why anything still queued at that moment was cleared in the
terminal and is dropped.

Editing a queued message in the terminal changes what runs without telling any hook, so
the pairing after that is by position and can attach a turn to the wrong prompt.

## When the resident is not there

A machine without the unit installed, or a resident that died, leaves the socket
unanswered. `Stop`, `StopFailure` and `Notification` then send from the hook process
itself, so the chat still gets the turn, as a message of its own with no draft before it
and no prompt above it to reply to. Nothing can be sent back to a session in that state.

## Checks

`./check.sh` runs shellcheck, formatting, clippy, the tests and a release build. The
lint levels live in `Cargo.toml`, so a bare `cargo clippy` and whatever an editor runs
in the background enforce the same set rather than only this script.

`tests/turn.rs` drives the hook chain end to end in a throwaway runtime directory,
against a server of its own that answers the way Telegram does. It asserts the calls a
two-segment turn makes, their order and which of them carries a notification, and that a
prompt queued during a turn gets a thread of its own.

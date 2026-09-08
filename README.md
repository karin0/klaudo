# klaude

Carries a Claude Code session's turns to a Telegram private chat and carries what you
type there back into the session's terminal, so a long turn can be left alone, picked up
from the phone, and answered from there.

## What lands in the chat

A turn opens with the prompt, quoted and without a sound, so a chat scrolled through
tells the asks from the answers by their shape alone. Every message the turn sends
afterwards is a Telegram reply to that one, so a chat that collects many turns reads as
a thread per turn. A prompt klaude typed is already in the chat as the message that
asked for it, and its turn threads under that message. A prompt deleted from the chat
leaves the rest of its turn arriving as messages of their own.

A turn is a sequence of segments. A segment is either an assistant message with text in
it or the run of tool calls between two of those. One message stands at the foot of the
turn showing the segment that is open, rewritten as that segment grows, and the segment
takes it once the next one starts. So the message the chat ends up holding is the one it
was watched in. Under what the open segment has said, that message carries a status
line, a word from Claude Code's own vocabulary and the turn's elapsed time, stepping to
the next word on every refresh, so the minutes a turn spends thinking are on screen as
they pass. It goes up three seconds into the turn, which leaves a turn answered at once
nothing to take back. The last segment keeps no message of its own: the `Stop` event
carries its text, so the message that was showing it is taken back once the answer is in
the chat.

A run of tool calls is posted a line per call: a mark for how it went, the tool, the
field of its input that says what it is doing, and the time it took. What a failed tool
reported goes on a line under that, its first sixty characters. A call a subagent made
carries that agent's type in brackets, and one still running is marked as such and shows
no time, so a run reads as the terminal does, and a run whose message went out with a
call still running is rewritten once that call reports. A run past thirty calls lists the
newest thirty and counts the rest.

● Bash `cargo test` 4s
× Bash `cargo clippy` 2s
⎿ Exit code 1
○ [Explore] Grep `fn seal`

The lines are ordinary text, so a long command wraps where a preformatted block would
have asked the reader to scroll sideways for the time at its end. The command itself
travels in a code span, which is what keeps a command carrying markdown from being read
as markdown.

Telegram's message drafts do the same job in one call, and klaude was built on them
first. A draft is ephemeral: it expires thirty seconds after its last frame, no method
retires it, and sending the message it was previewing leaves it standing. Clients differ
on what happens when the message arrives beside it, from a clean transition to a
duplicate to a crash. Rewriting a real message costs one extra call at the end of a turn
and none of that is possible.

An assistant message's last flush reaches the resident after the hook of the tool call
that message ends with, by tens of milliseconds, so a tool call waits a tenth of a
second before it is filed. That is long enough for the words introducing it to arrive
and take their place above it, and far shorter than the wait for anything a turn says
after a call has run. `PreToolUse` names no message, so the order comes from the clock
until it does.

A flush later than that lands after klaude has posted the message it belongs to, and a
tool can report once the run holding it is already a message. Both are written into the
message their segment became, which is why a turn keeps its segments and the message
each of them turned into until it ends.

So a turn that talked, worked and talked again leaves those three in the chat, in order,
and the last message is the only one that makes a sound. A `Notification` sounds too,
because a session stopped at a dialog is the other thing worth coming back to. Each
message carries the elapsed time it was posted at; only the last carries the `#claude`
tag, which therefore counts turns rather than segments.

Every message opens with the same line: the directory Claude Code files the session's
transcript under, then `session/prompt` shortened to eight characters each. That
directory is where the session was opened, so it stays put across a `cd` inside a turn.
The line is also the address a reply is routed by.

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
answered in the chat with the terminal it is on instead. A session started as a
background job is one of those: Claude Code gives it a pty of its own, so the tmux
window its output appears in belongs to the session that launched it.

## Hooks

One command answers every event, so `settings.json` repeats it under `SessionStart`,
`UserPromptSubmit`, `MessageDisplay`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
`Stop`, `StopFailure` and `Notification`:

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
integer id of a private chat, and it is the sender every incoming message is checked
against.

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
Three constraints put it there. The message a turn is watched in has to keep its clock
moving while nothing else happens. The answer must not race a rewrite still in flight,
and the replies all carry the id Telegram gave the prompt message; both are free once
one process issues every call in order.

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
itself, so the chat still gets the turn, as a message of its own with nothing shown
before it and no prompt above it to reply to. Nothing can be sent back to a session in that state.

`SessionStart` and the three tool events are dropped instead. A tool event posted on its
own would be one Telegram call per tool call, and `PreToolUse` holds up the call it
announces until the hook returns.

## Checks

`./check.sh` runs shellcheck, formatting, clippy, the tests and a release build. The
lint levels live in `Cargo.toml`, so a bare `cargo clippy` and whatever an editor runs
in the background enforce the same set rather than only this script.

The release build is what the hook runs, so `systemctl --user restart klaude` belongs
after it: the resident keeps the image it started with, and a hook newer than the
resident forwards events the resident has no arm for, which reach the chat as the
verbatim report an unrecognised event falls back to.

`tests/turn.rs` drives the hook chain end to end in a throwaway runtime directory,
against a server of its own that answers the way Telegram does. It asserts what the chat is left
holding after a two-segment turn, in order and with the sound each message carried; that
a segment watched while it ran finishes in the message it was watched in and the last
one's message is taken back; that a prompt queued during a turn gets a thread of its
own; that a run of tool calls is a message between the two halves of what the turn said;
that a call announced ahead of the words introducing it still
follows them; that a delta landing after its own `Stop` leaves the answer last; that a
flush and a
tool outcome arriving after their segment went out rewrite that message; and that a
message replying to nothing reaches the session heard from last.

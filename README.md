# klaude

Carries a Claude Code session's turns to a Telegram private chat or group and carries
what you type there back into the session's terminal, so a long turn can be left alone, picked up
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

A run of tool calls is posted a line per call: a mark for how it went, the tool, what
the call says it is doing, and the time it took. A tool whose input describes the call,
as Bash's does, gives that line its words and carries what the call works on, the
command, on a line under it; where nothing describes the call, that field stands on the
line itself. What a failed tool reported goes under those, its first sixty characters. A
call a subagent made carries that agent's type in brackets, and one still running is
marked as such and shows no time, so a run reads as the terminal does, and a run whose
message went out with a call still running is rewritten once that call reports. A run
past thirty calls lists the newest thirty and counts the rest.

● **Bash**  run the tests **4s**
⎿ `cargo test`
× **Bash**  lint everything **2s**
⎿ `cargo clippy`
⎿ Exit code 1
○ [Explore] **Grep**  `fn seal`

The tool and the time are bold, which is what the eye follows down a run whose middles
are of every length. The lines are ordinary text, so a long command wraps where a
preformatted block would have asked the reader to scroll sideways. What a call works on
travels in a code span, which is what keeps a command carrying markdown from being read
as markdown. A description reads as the sentence it is, and so does every other sentence
a person or a tool wrote, its markdown characters escaped on the way out. What Claude
Code answered is markdown and reads as markdown.

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

## Rich messages

A message is posted with `sendRichMessage` and rewritten with `editMessageText`, both
carrying the body in the `markdown` field of a `rich_message` parameter. Bot API 10.1
added the method in June 2026, and its markdown is a dialect of its own, documented at
<https://core.telegram.org/bots/api#rich-message-formatting-options>. Headings, tables,
footnotes, `==marked==`, `||spoiler||` and `$formula$` belong to it along with the
emphasis every markdown has, where `**text**` is bold and `*text*` is italic as
CommonMark has them; the `parse_mode` markdown of the older methods gives `*text*` to
bold.

A backslash in front of a character the dialect owns is consumed and the character
stands. In front of any other character it stays, and a client copying the message out
hands back the backslash with it, which is why `hook::prose` escapes against that set
alone. HTML tags are parsed inside this markdown, so the characters HTML owns travel as
entities.

A rich message holds 32768 characters and 500 blocks, against which a turn's longest
message is a few thousand characters of one paragraph.

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

`/new <directory>` posts an anchor naming that directory and starts nothing, and so
does `/new@<bot> <directory>`, which is how a group's command menu writes it. Replying to
the anchor opens a window running `claude` there and types the reply as its first
prompt. Those windows live in a tmux session called `klaude`, one window per
conversation, so `tmux attach -t klaude` reaches a conversation that began on the phone.

A session that opens a directory for the first time stops at the dialog asking whether
the folder is trusted, and reports what it is showing to the chat rather than typing
into a dialog. Answer that once locally and the directory stays trusted.

Only one person is answered: a message is acted on when `USER_ID` sent it, in `CHAT_ID`
or in that person's private chat with the bot. A turn a message started is posted in
the chat the message came from, and so is whatever klaude says back to a message; a
turn started in the terminal goes to `CHAT_ID` when its project is listed in
`CHAT_PROJECTS`, and to the private chat otherwise. In a group, a bot in Telegram's default privacy mode receives only commands
and replies to its own messages, so a message replying to nothing reaches klaude only
once privacy mode is turned off with BotFather's `/setprivacy` or the bot is made an
admin. A channel is not supported: a post there carries no sender to check.

## Sending a file

`klaude send <file>` posts a file as a document in the thread of the turn that ran the
command, below what the turn has said so far, and in the chat of the session's project
between turns. Claude Code puts `CLAUDE_CODE_SESSION_ID` in the environment of every
command it runs, which is how the command finds its session. Run anywhere else, the
command sends the file without a caption to the chat of the directory it runs in. Claude learns the command from whatever instructions
it reads, such as a line in `~/.claude/CLAUDE.md`.

The command hands the path to the resident and waits for the upload to finish, so its
exit status says whether the file reached the chat, and a failure prints what Telegram
answered. A bot uploads files of up to 50 MB.

A document carries no rich message, so its caption is the head in Telegram's HTML, and a
reply to the file reaches the session like a reply to any message of the turn.

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

Delivery ends whatever mode the pane is in first, which brings a pane scrolled up back
to the bottom. A paste reaches the input box from copy mode, while the Enter after it
goes to that mode's own key table and leaves the text sitting in the box.

A session running outside tmux has no pane to type into, and a reply aimed at one is
answered in the chat with the terminal it is on instead. A session started as a
background job is one of those: Claude Code gives it a pty of its own, so the tmux
window its output appears in belongs to the session that launched it.

## Hooks

One command answers every event, so `settings.json` repeats it under `SessionStart`,
`UserPromptSubmit`, `MessageDisplay`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
`Stop`, `StopFailure` and `Notification`:

```json
{"type": "command", "command": "exec <path to klaude>"}
```

The hook writes the event to a unix datagram socket and exits. Along with the event it
carries `$TMUX`, `$TMUX_PANE` and its own parent process id, which is where the resident
learns which terminal a session is on. The parent is the session because of `exec`: it
hands the shell Claude Code starts the command in over to the binary, which leaves the
session as the parent the binary reports.

`SessionStart` fires once the session is ready for input, after the trust dialog, so it
is both how a session announces where it lives and how a conversation opened from the
chat knows when to type its first prompt.

## Configuration

`BOT_TOKEN`, `CHAT_ID` and the optional `USER_ID` and `CHAT_PROJECTS` come from
`$XDG_CONFIG_HOME/klaude/env`, which defaults to `~/.config/klaude/env`. That file is the whole of where they come from, so a token
changed there is the token every session uses from its next event on, and a value
exported in a shell reaches nothing. A file that cannot be read, or a name missing from
it, stops the process and names the file. `CHAT_ID` is the integer id of the chat, and
`USER_ID` is the integer id of the person klaude answers. A private chat's id is its person's id, so `USER_ID` defaults to
`CHAT_ID`. A group's id is negative, and a group `CHAT_ID` without `USER_ID` stops the
process. A message klaude ignores is logged with its chat's id and title
and its sender's id, so `journalctl --user -u klaude` after a message sent in a group
shows the ids to write here.

`CHAT_PROJECTS` lists the projects whose turns started in the terminal go to `CHAT_ID`,
as absolute directories separated by `:` the way `PATH` is written. A project is the
directory a session was opened in, the one its messages are headed with, and it is
listed when it is one of those directories or inside one. Every other project goes to
the private chat, which is all of them when the name is absent. A relative directory
in the list stops the process.

`dotenvy` reads the file: `NAME=value` lines, `#` opening a comment, an `export` in
front allowed, and a `$` expanding outside single quotes, so a file written for a shell
to source reads the same way here.

`API_BASE` is optional and defaults to `https://api.telegram.org`; the test writes it
into a file of its own, pointing at a server of its own.

`klaude.service` runs the resident and reads the same file:

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
unanswered. `UserPromptSubmit`, `Stop`, `StopFailure` and `Notification` then send from
the hook process itself, so the chat still gets the ask and the answer, each as a
message of its own with nothing shown before it and no prompt above it to reply to.
Nothing can be sent back to a session in that state.

`SessionStart`, the three tool events and `MessageDisplay` are dropped instead. Each of
them says something only as part of what the resident is assembling, so posted alone it
would be one Telegram call per tool call and per streamed fragment, and `PreToolUse`
holds up the call it announces until the hook returns.

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
tool outcome arriving after their segment went out rewrite that message; that a file
`klaude send` posts replies to the prompt of its turn, one from a session klaude has not
heard from fails the command, and one sent outside Claude Code from a directory not in
`CHAT_PROJECTS` goes to the private chat; that `--help` and a call the binary cannot
act on print the usage, and a missing file or a stopped resident is named without a
panic; and
that a
message replying to nothing reaches the session heard from last, while the same message
from anyone else in the group gets no answer, and the answer to the user's own goes to
the chat it was sent in.

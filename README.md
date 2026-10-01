# Klaŭdo

Posts a Claude Code session's turns to a Telegram private chat or group and types the
replies you send there into the session's terminal, so a long turn can be left alone,
followed on the phone, and answered from there.

Klaŭdo is Esperanto for Claude. Its commands, paths and names in the system are spelled
`klaudo`, the Esperanto h-system's spelling of ŭ as u.

## Chat messages

A turn opens with its prompt, quoted and silent, and every later message of the turn
replies to it, so the chat reads as a thread per turn. A prompt Klaŭdo typed is already
in the chat as the message that asked for it, and its turn threads under that one.

A turn is a sequence of segments, each an assistant message's text and the run of tool
calls it goes on to make, under that text in the same message. A run that would push
the message past Telegram's limit goes on in a message of its own. The open segment is
shown in one message at the foot of the turn, rewritten as it grows, with a status line
of a word from Claude Code's vocabulary and the elapsed time. In a group it is rewritten
at most every ten seconds, because Telegram counts a rewrite against the twenty messages
a minute a bot may send there. A `date_time` entity would keep the time current without
rewrites, but clients show one inside a rich message as its fallback text.

The last segment keeps no message of its own: `Stop` carries its text, and the message
showing it is taken back once the answer is posted. That text reaches the daemon
milliseconds before `Stop`, so new text waits a tenth of a second of quiet before it is
shown. A turn that ends on a run keeps that last message, and the answer repeats its
text. Only the answer and a `Notification` make a sound, and only the answer carries
the `#claude` tag.

A run of tool calls is a line per call:

● **Bash**  run the tests **4s**
⎿ `cargo test`
× **Bash**  lint everything **2s**
⎿ `cargo clippy`
⎿ Exit code 1
○ [Explore] **Grep**  `fn seal`

Klaŭdo puts a call's command, path or pattern in a code span, so markdown inside it
stays literal, and escapes the markdown in every other sentence a person or a tool
wrote. Claude Code answers in markdown, and Klaŭdo posts that markdown as it is.

The calls that finished ahead of the first one still running fold into one expandable
quotation once their descriptions and subjects together run past 120 characters, three
lines of a phone's screen. The running calls stay under it in view. Markdown isn't
parsed inside a block HTML tag, so the folded calls are written in HTML, and a blank
line keeps the quotation apart from the lines under it.

Telegram's message drafts would do the open segment's job in one call, and Klaŭdo was
built on them first. A draft expires thirty seconds after its last frame, no method
retires it, and clients differ on what they do when the real message arrives beside it,
from a clean transition to a duplicate to a crash.

Claude Code fires `MessageDisplay` for the words introducing a tool call about a second
after that call's `PreToolUse`, and `PreToolUse` names no message, so nothing orders the
two. A call therefore waits a tenth of a second before it is filed. A flush or a tool
outcome arriving later than that is written into the message its segment became, which
is why a turn keeps its segments until it ends. Narration Claude Code shows is a
thinking block in the transcript, fires no `MessageDisplay`, and stays out of the chat.

`/compact` runs no turn, so its `PostCompact` is the answer to it, tagged
`#claude #compact`. A compaction Claude Code starts itself is a silent `#compact` message
in its turn's thread. Both quote the summary and the reasoning ahead of it, folded.

Every message opens with the directory Claude Code files the session's transcript under,
then `session/prompt` shortened to eight characters each. That line is the address a
reply is routed by.

## Rich messages

A message is posted with `sendRichMessage` and rewritten with `editMessageText`, both
carrying the body in `rich_message.markdown`. Bot API 10.1 added the method in June 2026,
and its markdown is a dialect of its own, documented at
<https://core.telegram.org/bots/api#rich-message-formatting-options>. `**text**` is bold
and `*text*` italic, as in CommonMark.

A backslash in front of a character the dialect owns is consumed. In front of any other
character it stays, and a client copying the message hands it back, which is why
`hook::prose` escapes against that set alone. HTML tags are parsed inside this markdown,
so the characters HTML owns travel as entities.

## Replies and commands

A reply to any message from a turn is typed into that session's terminal as a prompt. A
message replying to nothing goes to the session Klaŭdo heard from last in the chat and
topic it was sent in. A session is where its running turn is posted, and between turns
in its project's chat, in the topic its last message went to. A session that has not
posted there yet takes the topic of the session of its project heard from last, so a
conversation restarted in a project stays in its topic.

The text travels through a tmux paste buffer, so newlines, quotes and non-ASCII arrive
as typed. It is pasted in pieces of at most three lines and 700 UTF-16 units, because
Claude Code folds a longer paste into a `[Pasted text #N]` placeholder and submits it
wrapped as text the user did not write. A message whose text reached an input box gets
a 👀 reaction once Claude Code reports the prompt.

A reply to a session that has exited opens a window running `claude --resume` in the
directory it ran in and types the reply once it is ready. The daemon remembers the
directories of the last thousand exited sessions.

`/new <directory>` posts an anchor, and replying to it opens a window running `claude`
there with the reply as its first prompt. `/new` alone offers a menu of the chat's
projects, led by the project of the session the message would reach. The windows live
in the tmux session `klaudo`, so `tmux attach -t klaudo` reaches a conversation that
began on the phone. An anchor carries `ForceReply`, which Telegram attaches only to a
message being sent, so an anchor is always a message of its own.

tmux runs `claude` through its `default-shell` as a non-interactive shell with the tmux
server's environment, so `claude` has to be on the `PATH` that shell ends up with. A
directory added only by an interactive shell's startup file, as the native installer's
`~/.local/bin` often is, makes the window exit at once, and the daemon answers that the
window closed before its session started.

A session that opens a directory for the first time stops at the trust dialog and
reports what it shows to the chat. Answer it once locally and the directory stays
trusted.

`/resume` goes straight to the sessions of the project the message would reach, with a
button back to the menu of projects, which it offers first when the message reaches no
session. The anchor it posts replies to the last message the session left, since a
private chat has no link to a single message. The anchor moves the session to where it
is posted, without a reply: a running turn goes on there under the anchor, leaving what
it already posted behind, and so does every later one. Prompts already queued in the
terminal stay where they were posted, since their turns reply to them.

`/usage` answers with the plan's limits and the context of the session a message would
reach, from what the sessions' status lines last reported. Claude Code draws `/usage` as
a dialog that takes every key until dismissed, which is why the daemon answers it. The
message showing a running turn and the answer closing it end with the same figures, as
in `5% 45.6k/1m · 1% 3h30m · 56% 2d14h`.

`/compact` goes to a session like any other message. The daemon lists these commands
in the command menu for the user alone.

Only `USER_ID` is answered, in `CHAT_ID` or in that person's private chat with the bot.
A turn a message started is posted where the message came from. A turn started in the
terminal goes to `CHAT_ID` when its project is in `CHAT_PROJECTS`, and to the private
chat otherwise. In a group, a bot in Telegram's default privacy mode receives only
commands and replies to its own messages, so an unaddressed message reaches Klaŭdo only
once privacy mode is off in BotFather's `/setprivacy` or the bot is an admin. A channel
is unsupported, since a post there carries no sender to check.

## Topics

A private chat is split into topics once topic mode is on for the bot in BotFather, and
a group is when it is a forum. Every message Klaŭdo sends names its topic, because
Telegram puts a message naming none outside every topic, even a reply to a message
inside one. A message in a forum's topic replying to nothing arrives replying to the
service message that opened the topic, which Klaŭdo reads as replying to nothing.

A private chat in topic mode takes no message outside every topic: one sent there opens
a topic whose name the service message marks as implicit. A message replying to nothing
in such a topic, when no session is there, goes to the session heard from last outside
every topic, which is where a turn started in the terminal is posted until a session of
its project posts in a topic. A topic the user named reaches only its own sessions.

Topic mode slows the whole private chat. A reply to `/new` shows several seconds later
where topic mode is on than where it is off.

## Sending a file

`klaudo send <file>...` posts the files as documents in the thread of the turn that ran
it, up to ten per album, the first captioned with the head so a reply to it reaches the
session. Claude Code puts `CLAUDE_CODE_SESSION_ID` in the environment of every command
it runs, which is how the command finds its session. Run elsewhere, it sends to the chat
of the directory it runs in. A line in `~/.claude/CLAUDE.md` pointing at `klaudo --help`
is enough for Claude to learn it.

The command asks the daemon where the files go and uploads them itself, so its exit
status says whether every file arrived. The answer comes to an abstract socket address,
which any local user can send to, so the command takes only an answer sent from the
daemon's own socket.

## Keystroke delivery

Claude Code's own local messaging socket delivers text to a running session too, but
labels it as coming from another Claude session, to be treated as a peer's request and
never as the user's approval. That guardrail against permission laundering is why Klaŭdo
types through `send-keys`, the path a person's own typing takes.

Before every delivery, the terminal `/proc/<pid>/stat` names for the session has to be
the one tmux reports for its pane, since a session that exited leaves the pane to a
shell, where the text would run as a command. Delivery first leaves copy mode, which
would otherwise take the Enter and leave the text sitting in the box.

A session outside tmux, including one started as a background job, which Claude Code
gives a pty of its own, has no pane, and a reply to it is answered with the terminal it
is on.

## Hooks

`exec klaudo` answers every event, and `klaudo status` is the status line; `settings.json`
holds both, to be merged into `~/.claude/settings.json`. The binary is installed as
`/usr/local/bin/klaudo`, on the `PATH` Claude Code runs hooks with.

The hook writes the event to the datagram socket `$XDG_RUNTIME_DIR/klaudo/listen.sock`
with `$TMUX`, `$TMUX_PANE` and its parent's pid, and exits. Because of `exec`, that
parent is the session itself. The status line is the one place Claude Code reports the
context and the plan's limits, so `klaudo status` forwards it and prints nothing.

`SessionStart` fires once the session is ready for input, after the trust dialog, which
is how a window opened from the chat knows when to type its first prompt.

## Configuration

`BOT_TOKEN`, `CHAT_ID` and the optional `USER_ID` and `CHAT_PROJECTS` come from
`$XDG_CONFIG_HOME/klaudo/env` alone, read with `dotenvy` on every event, so a changed
token applies from the next event and an exported variable reaches nothing. A private
chat's id is its person's, so `USER_ID` defaults to `CHAT_ID`, and a group, whose id is
negative, needs it set. A message Klaŭdo ignores
is logged with its chat's and sender's ids, so `journalctl --user -u klaudo` shows what to
write here.

`CHAT_PROJECTS` lists absolute directories separated by `:`. A project inside one of
them posts its terminal-started turns to `CHAT_ID`.

`TRACE_UPDATES`, set to anything, logs every polled update as Telegram sent it.
`API_BASE` defaults to `https://api.telegram.org`, and the tests point it at their own
server.

```sh
sudo ln -s "$PWD/target/release/klaudo" /usr/local/bin/klaudo
mkdir -p ~/.config/klaudo && ln -s <secrets file> ~/.config/klaudo/env
systemctl --user enable --now "$PWD/klaudo.service"
```

The link points at the release build, so a rebuild updates what the hooks run, and
`systemctl --user restart klaudo` updates the daemon. A hook newer than the daemon
forwards events it has no arm for, which reach the chat verbatim.

## The daemon

The terminal draws streamed text only once the `MessageDisplay` hook returns, so the
hook cannot touch the network. That is why the hook is a compiled binary: shell scripts
cost 9.4 ms per invocation against 0.5 ms for the same handoff.

A daemon per machine does the rest. The open segment's clock has to move while
nothing happens, the answer must not race a rewrite still in flight, and Telegram hands
updates to one reader per bot, who has to answer when no turn is running. Routing a
reply needs to know which session a message belongs to, so the reader is the process
that posts.

A reply is routed by the address read back out of the message it replies to, so a
restarted daemon still routes replies to messages it never posted. The daemon writes
the pane of each session to `$XDG_RUNTIME_DIR/klaudo/state.json` as it changes, so an
idle session stays reachable across a restart, and the directory of each exited one to
`$XDG_STATE_HOME/klaudo/ended.json`, so a reply resumes it after a reboot too. The
daemon keeps that record itself, since the transcripts under `~/.claude/projects` are in
a format Claude Code has not published. A turn in flight is lost.

A session is forgotten at its `SessionEnd`, or once `/proc/<pid>` is gone for one that
was killed first.

A call Telegram rejects with a rate limit or its own failure is retried up to three
times, so a message can arrive late, or twice when the answer to an attempt was lost.

## Queued prompts

`UserPromptSubmit` for a prompt submitted during a turn carries the running turn's
`prompt_id`, and the queued turn's own id appears only once it begins. So such a prompt
is posted and queued, and a turn with a new id takes the oldest queued message. A prompt
submitted with nothing running drops the queue, which the terminal has cleared. Editing
a queued message in the terminal tells no hook, so the pairing is by position and can
attach a turn to the wrong prompt.

## Hooks without a daemon

With no daemon listening, `UserPromptSubmit`, `Stop`, `StopFailure`, `Notification` and
`PostCompact` send from the hook process itself, each as a message of its own, and
nothing can be sent back. Every other event is dropped, since posted alone it would be
one call per tool call or streamed fragment, and `PreToolUse` holds up its call until
the hook returns.

## Checks

`./check.sh` runs shellcheck, formatting, clippy, the tests and a release build. The lint
levels live in `Cargo.toml`, so a bare `cargo clippy` enforces the same set.
`tests/turn.rs` drives the hook chain end to end in a throwaway runtime directory,
against a server answering as Telegram does and a `tmux` that records how it was called.

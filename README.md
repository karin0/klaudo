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
of a word from Claude Code's vocabulary and the elapsed time. Text growing past what that
message holds leaves it as it last fitted, and the complete segment takes its place as a
markdown file. In a group it is rewritten at most every ten seconds, because Telegram
counts a rewrite against the twenty messages a minute a bot may send there. A
`date_time` entity would keep the time current without rewrites, but clients show one
inside a rich message as its fallback text.

While a turn runs, the bot shows as typing in the turn's topic, and the chat list shows
that too, so a topic with a turn running stands out without being opened. Telegram shows
a chat action for five seconds, through rewrites but only until the bot's next message,
so the daemon sends one every four and again after each message the turn posts. A
session waiting on a dialog shows none.

`/stop` sends Escape to the pane of the session a message would reach while it runs a
turn, after the check a reply goes through. A button on the message showing the turn
would be easy to press by mistake, so stopping takes a command. An interrupted turn
fires no hook, not even `PostToolUse` for the tool call it cut short. What marks it is
the user message `[Request interrupted by user]` that Claude Code appends to the transcript, in its own
unpublished format, so the daemon reads the transcript's new lines for five seconds. It
ends the turn with a silent `#interrupted` message once that line appears, and says in
the thread that the turn did not stop otherwise. Escape also cancels an open dialog, and
a second Escape on an idle prompt opens the `/rewind` picker, so a `/stop` while the
daemon still waits on the last one sends nothing. A background shell
survives the interrupt and starts a turn of its own once it exits.

The last segment keeps no message of its own: `Stop` carries its text, and the message
showing it is taken back once the answer is posted. That text reaches the daemon
milliseconds before `Stop`, so new text waits a tenth of a second of quiet before it is
shown. A turn that ends on a run keeps that last message, and the answer repeats its
text. Only the answer and a `Notification` make a sound, and only the answer carries
the `#claude` tag.

A run of tool calls is a line per call:

● **Bash**  run the tests **4s**
⎿ `cargo test`
⎿ test result: ok. 58 passed
× **Bash**  lint everything **2s**
⎿ `cargo clippy`
⎿ Exit code 1
○ [Explore] **Grep**  `fn seal`

Klaŭdo puts a call's command, path or pattern in a code span, so markdown inside it
stays literal, and escapes the markdown in every other sentence a person or a tool
wrote. Claude Code answers in CommonMark, and Klaŭdo posts that markdown with escapes
only where Telegram's dialect reads it differently.

A finished call sums up its output in a line, as Claude Code's terminal does: the lines
a file read or an edit changed, or the files a search matched. A command shows the last
five lines it printed, and a failure the first five lines of its error, while they stay
in view as the next paragraph describes. An MCP tool follows no schema Klaŭdo could read
a subject or a summary from, and its arguments and output are usually what a reader
opens the call for, so its call shows both nearly whole. Printed text goes in a code
span per line, since a phone scrolls a code block sideways and wraps a span. Telegram
drops the spaces a span opens with, so a line's indentation goes in ideographic spaces
of two columns each, which it keeps.

While a run is the open segment, the calls from the first one still running to the end
stay in view, or else the latest call alone, since they are what a reader follows a turn
by, and their commands and summaries read more than twice as far as the others', which
show one line of what a command printed or an error said. The calls ahead of them, and
every call of a run the turn has moved on from, fold into one expandable quotation once
everything they show together runs past 120 characters, three lines of a phone's screen.
Markdown isn't parsed inside a block HTML tag, so the folded calls are written in HTML,
and a blank line keeps the quotation apart from the lines under it.

Telegram's message drafts would do the open segment's job in one call, and Klaŭdo was
built on them first. A draft expires thirty seconds after its last frame, no method
retires it, and clients differ on what they do when the real message arrives beside it,
from a clean transition to a duplicate to a crash.

Claude Code fires `MessageDisplay` for the words introducing a tool call about a second
after that call's `PreToolUse`, and `PreToolUse` names no message, so nothing orders the
two. A call therefore waits a tenth of a second before it is filed. A flush or a tool
outcome arriving later than that is written into the message its segment became, which
is why a turn keeps its segments until it ends. The narration Claude Code shows above
a tool call fires no hook. Claude Code writes it to the transcript as a thinking block
ahead of the call, and writes the model's reasoning empty, so a call being filed reads
the transcript's new lines for its narration, which opens a segment as a message's words
do.

`/compact` runs no turn, so its `PostCompact` is the answer to it, tagged
`#claude #compact`. A compaction Claude Code starts itself is a silent `#compact` message
in its turn's thread. Both quote the summary and the reasoning ahead of it, folded.
The transcript keeps only the summary, so the hook also appends each compaction's whole
output, with its time in Unix milliseconds and its trigger, as a line of JSON to
`$XDG_STATE_HOME/klaudo/compactions/<session_id>.jsonl`.

Every message opens with the directory Claude Code files the session's transcript under,
then `session/prompt` shortened to eight characters each. That line is the address a
reply is routed by.

## Rich messages

A message is posted with `sendRichMessage` and rewritten with `editMessageText`, both
carrying the body in `rich_message.markdown`. Bot API 10.1 added the method in June
2026, and its markdown is a dialect of its own, documented at
<https://core.telegram.org/bots/api#rich-message-formatting-options>. `**text**` is bold
and `*text*` italic, as in CommonMark, but a `#` starting a line opens a heading even
with no space after it, so Klaŭdo escapes such a `#` in Claude Code's answers. A rich message holds up to 500 top-level blocks,
and text rendering to 32768 characters against the 4096 of `sendMessage`. Telegram
rejects longer text, but first keeps about 35000 bytes of it in UTF-8 and drops the rest
unannounced, so text in Chinese is cut near 11700 characters without an error. A body is
measured in bytes of its markdown, which markup only lengthens.

A body past 32768 bytes or 500 top-level blocks goes whole as `message.md`, a document
captioned with its title in HTML, so a reply to it reaches the session as a reply to any
message does. Clients render a markdown file in their in-app browser, where the desktop
client shows HTML tags as their source, so the folded quotations stay within a rich
message. The body stays one file because splitting it over rich messages would need a
parser of the dialect to find where no construct is left open, as a `<details>` holding
blank lines is, and a code block, whose text Telegram does not parse, scrolls sideways
on every line. The size is decided before sending, since Telegram accepts a long body in
Chinese and cuts it.

A backslash in front of a character the dialect owns is consumed. In front of any other
character it stays, and a client copying the message hands it back, which is why
`hook::prose` escapes against that set alone. HTML tags are parsed inside this markdown,
so the characters HTML owns travel as entities.

## Replies and commands

A reply to any message from a turn is typed into that session's terminal as a prompt. A
message replying to nothing goes to the most recent session in the chat and topic it was
sent in, which is resumed first when it has exited. A session is as recent as the last
message of its conversation, its own or one typed into it, and the menus order sessions
and projects the same way. The last event a session reported only starts its idle time,
since a session reports events the chat never shows. A session is in its project's chat,
in the topic its last message went to. A session that has not posted there yet takes the
topic of the most recent session of its project, so a conversation restarted in a project
stays in its topic.

The text travels through a tmux paste buffer, so newlines, quotes and non-ASCII arrive
as typed. It is pasted in pieces of at most three lines and 700 UTF-16 units, because
Claude Code folds a longer paste into a `[Pasted text #N]` placeholder and submits it
wrapped as text the user did not write. A message whose text reached an input box gets
a 👀 reaction once Claude Code reports the prompt.

A photo or a file sent in the chat is saved under `$XDG_STATE_HOME/klaudo/files/`, and
the prompt is its caption followed by the path of each file on a line of its own, in a
code span, since Claude Code turns a paste of one image's path alone into an attachment
and reports the prompt as `[Image #N]`. Claude opens an image it names with its Read
tool. Telegram delivers each file of an album as a message of its own, which the daemon
gathers into one prompt. The files are kept as long as the transcripts whose prompts
name them. A message carrying a file is never one of the daemon's commands.

A dialog in the terminal drops pasted text and takes the Enter after it as its default
choice, which approves a permission prompt's tool call. So while a session waits on a
dialog, from its `Notification` to the next event it reports, a message to it is typed
nothing and answered with `/attach` and `/stop`, which reach the dialog. A dialog
cancelled by Escape in the terminal reports nothing, so the session takes no message
from the chat until it next reports.

A reply to a session that has exited opens a window running `claude --resume` in the
directory it ran in and types the reply once it is ready. The daemon remembers the
directories of the last thousand exited sessions.

A message to a running session outside tmux, such as one in an editor, has no terminal
to be typed into, so the daemon answers it with a button. Pressed by the message's
sender, the button opens a window running `claude --resume --fork-session`, which copies
the conversation into a new session and leaves the original running, and types the
message there once the branch is ready. The branch stays in the original's topic, and
works on the same files as the original.

`/new <directory>` posts an anchor, and replying to it opens a window running `claude`
there with the reply as its first prompt. `/new` alone offers a menu of the chat's
projects, led by the project of the session the message would reach. The windows live
in the tmux session `klaudo`, so `tmux attach -t klaudo` reaches a conversation that
began on the phone. The first window starts a tmux server when none is running, and
that server stays in the daemon's cgroup, where a restart of the daemon kills it with
every session it holds, so a machine should start its server under a unit of its own
ordered before the daemon. A window there whose session has been unheard from for `IDLE_HOURS`
between turns is closed, to free its memory, unless a process the session started is
still running, and a reply resumes the session like any other that exited. An anchor carries `ForceReply`, which Telegram attaches only to a
message being sent, so an anchor is always a message of its own.

`/clear` posts the anchor of the project of the session the message would reach, which
is the first choice `/new` alone offers, without the menu. Telegram refuses to edit a
message carrying `ForceReply`, so an anchor takes no button back to the projects.

A client reopens the reply box of a topic's latest anchor whenever the topic is entered,
until a later message of the bot there sets another reply markup, and then a message
typed there goes to that anchor's session. `/cancel` posts a message removing the
markup, and so does a reply to the anchor of `/new` or `/clear` once its window opens,
since every message that anchor's box catches would open another conversation. The box
of another anchor sends a message to the session just picked, so it stays until
`/cancel`. The message removing the markup has to stay, since deleting it brings the
anchor's box back. The mobile clients apply a new reply markup, `ForceReply` included,
as it arrives, while Telegram Desktop applies it only once the topic is entered again.

tmux runs `claude` through its `default-shell` as a non-interactive shell with the tmux
server's environment and the daemon's `PATH`, so `claude` has to be on the `PATH` that
shell ends up with after its startup files. A
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
in `45.6k/1m 99% · $1.23 · 1% 3h30m · 56% 2d14h`, where `99%` is the share of the turn's
first call that Claude Code's transcript shows read from the prompt cache. Later calls of a
turn follow within seconds and nearly always hit the cache, while the first one misses it
once the cache has expired since the turn before. `$1.23` is what the session's calls, its
subagents' included, would cost at the API's list prices, except the call that compacts the
conversation, which the transcript does not record. It is counted in memory from when the
daemon first heard of the session, so it starts over when the daemon restarts or the session
is resumed. Anthropic does not publish how a plan's limits are spent, so the figure only
compares turns and sessions with each other. The prices are written in the code, since the
few models Claude Code runs change seldom, and a session that called a model missing from
them shows no cost from then on. When Claude Code resumes a session it
writes the earlier entries of the transcript again, and the cost leaves them out by their
timestamps. Claude Code writes a turn's last call to the transcript only after the Stop
hooks have run, so the answer closing a turn waits up to two seconds for the transcript to
record the turn's end. A session in the VS Code extension draws no
status line, so its context shows as the tokens alone, which the transcript records of
each call. The daemon reads them from where the transcript ended when it first
heard of the session, since the transcript of a long conversation runs to tens of
megabytes.

`/diff` answers with the unstaged changes of the working tree holding the directory of
the session a message would reach, each file drawn in pictures folded under a line of
its status and line counts. Telegram's markup has no colours, so `pango-view`, from the
pango package, draws them, and has to be on the daemon's `PATH`. A file changed in too
many lines to read on a phone, as a lock file is, is listed without being drawn, and so
is a binary one.

`/attach` shows the terminal of the session a message would reach on another device,
for the dialogs a reply cannot answer, such as the `/rewind` picker. It runs the program
`ATTACH_COMMAND` names with the session's tmux pane and the socket of its tmux server as
its two arguments, and marks the message with 👀 once the program succeeds. A session
that has exited is resumed in a window first, which the terminal then shows starting.
Configuration below has such a program for a phone running Termux.

Of Claude Code's own commands, `/compact`, `/branch`, and `/model`, `/effort` and
`/rename` with an argument go to a session like any other message, since they run without
a dialog. Any other message starting with a slash is typed with its first word in
backticks, so Claude reads it as text and invokes a skill it names through its Skill tool,
while a mistyped command, or one opening a dialog such as `/model` alone, opens nothing in
the terminal. A command Claude Code adds later reaches Claude this way until the list in
the code includes it. The daemon's own commands other than `/new` take no argument, so one
followed by text, such as `/resume the design`, goes to a session the same way. The daemon
lists these commands in the command menu of each person it answers, shown to that person
alone.

Only `USER_ID` is answered, in `CHAT_ID` or in that person's private chat with the bot,
and the guests in `GUEST_IDS`, in `CHAT_ID` alone. A guest reaches only the sessions and
directories inside `CHAT_PROJECTS`, whichever message, anchor, menu or button names
them, and is answered `that project is not open to guests` otherwise. Whoever can type
into a terminal can do as much as its shell, so the grant itself, who and which
projects, is the boundary. Within it, the daemon types each guest's text inside
`<guest>`, behind a `<system-reminder>` that names the guest, says how to address them,
and limits Claude to routine development in the session's directory, away from the files
outside it other than the toolchain's and the files the guest sent with the prompt, the
system's state and the local network. Each `@` a guest writes reaches the terminal as
`&#64;`, since Claude Code would attach the file it names before Claude reads the
reminder. A guest's `/compact` and `/branch` are typed as they were sent, since
compacting and branching are routine work. Either with arguments follows the reminder,
since instructions to `/compact` would write the summary every later turn reads. Their
other slash commands follow the reminder, so Claude Code reads them as text, which keeps
`/model`, `/permissions` and the like out of a guest's reach. `/attach` would hand over
the terminal itself, so a guest's menu leaves it out and the daemon refuses it to them.

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

A private chat in topic mode takes no message outside every topic, since one sent there
opens a topic of its own. A turn started in the terminal is posted outside every topic
until a session of its project posts in a topic, so a reply to one of its messages or
`/resume` brings the session into a topic. A topic reaches only its own sessions, so a
message sent outside every topic by mistake reaches no session.

`/split` opens a topic for the session a message would reach, named after its latest
prompt, and moves the session there the way `/resume` does. Claude Code's `/branch` starts
a new session, the branch. Where `/branch` was sent from a topic, the daemon opens a
topic for the branch under the name Claude Code gave it, or else its latest prompt, and
answers `/branch` with a link to that topic. Elsewhere it answers with the branch's head,
which the next reply reaches. A branch made in the terminal stays in the topic of the
session it branched from. A session in a topic opened for it still counts toward the
topic it left, so a new session of its project goes there, and each topic keeps one
conversation. In a group, Telegram lets the bot open a topic only with the
`can_manage_topics` right.

`/rewind` copies the conversation of the session a message would reach into a new
session, up to the end of the turn of the message it replies to. Claude Code's own
`/rewind` picker goes back to before the prompt picked, but a prompt sent from the chat
is the sender's own message, which carries no head to reply to, so a reply picks the
answer it was given and keeps it. A message that names no turn, such as one replying to
nothing, keeps the conversation up to the end of its latest compaction, which is also
what a reply to the compaction's summary keeps. The copy's head is posted where the
message was sent, over the first prompt left out, as the picker hands that prompt back,
so a reply resumes the copy there and the original stays as it was. A copy wanting a
topic of its own takes `/split`. The picker takes only arrow keys, and
`--resume-session-at` truncates a resumed conversation only in print mode, so the daemon
writes the copy's transcript itself: the original's entries up to the next prompt, each
naming the new session, in the file beside the original's, where `claude --resume` looks
for it. The turn is found by the prompt id that a message's head shortens, which the
transcript records on every prompt. Only the conversation
goes back, so the copy works on the files as they are. The daemon keeps the transcript
path each session's hooks named, since a session that has exited reports none.

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
status says whether every file arrived.

## Reviews

Claude Code links to the files it talks about, as in `[listen.rs:588](src/listen.rs#L588)`.
Once `WEB_LISTEN` is set, each inline link of an answer whose target has no scheme opens
the file in the bot's main Mini App, which the daemon serves, at the lines a fragment
`#L12` or `#L12-L20` names. The link is `https://t.me/<bot>` with
`?startapp=<session>_<from>_<to>_<path in base64url>`, so it carries everything the page
needs and still opens after a restart. The daemon reads the bot's username from `getMe`
when it starts. A link whose parameter would pass the 512 characters Telegram passes on
stays as Claude Code wrote it. A relative path resolves in the session's directory. An
answer the hook posts without a daemon keeps its links as Claude Code wrote them.

A turn of a session in a git working tree that changed anything there ends with a link
to the changes, as in `[Review changes: 3 commits, 5 files](https://t.me/<bot>?startapp=<session>_<commit>)`.
The daemon records the commit checked out as the turn starts, and the page shows the
changes from it to the working tree as it is when the page opens, untracked files
included, so an older link shows everything since its turn began. The link is in the
answer's text, since the daemon rewrites a message without its buttons, and an answer
split over several messages would have to carry a button on its last one. The page
lists the commits the changes take in, and a tap on any commit moves the start to its
parent, and lists the files, each scrolling the page to itself on a tap. Each file shows
the hunks git found, and expanding one loads both sides of the file whole. The arrow in a
file's header folds it.

On either page, a tap on a line number opens a comment there, and a tap on another in
the same file stretches it over the lines between, as a drag across line numbers does.
A comment on the changes may span removed lines, which the prompt names as git names a
file as of a commit, as in `0123456789ab:src/a.rs:3-4`, so Claude does not read them as
lines of the file as it is. `Suggest change` fills an editor with the lines to rewrite,
and only lines of the file as it is can be rewritten. The top of the page lists the
comments saved, each scrolling the page to its lines on a tap. `Send review` types every
comment into the session as one prompt, the way a reply from the chat is typed, so a
guest's review goes behind the reminder and a session waiting on a dialog refuses it. The
daemon first posts the prompt quoted under the session's head where the session posts
between turns, and the turn it starts replies to that message, as a turn replies to the
message that asked for it.

Telegram signs the `initData` it hands the page with a key derived from the bot token.
It names who opened the page and carries the link's `startapp`, and the page passes it
with every request. That signature is all that guards the files, so the page reaches as
far as a reply could: the user reads any file the daemon can, and a guest only the
sessions and the files inside `CHAT_PROJECTS`, with symlinks resolved, and the changes of
a working tree whose top is inside them. A signature older
than a day is refused, and opening the link again signs a new one. A file past 1 MiB or
not in UTF-8 is refused.

The page is a React app in `web/`, which pnpm builds into `web/dist` for the binary to
embed, so the page always matches the daemon serving it. It draws files and diffs with
`@pierre/diffs`.
The server runs tokio and hyper on a thread of its own, and asks the main loop for what
only the main loop knows, a session's directory and pane, through the channel the other
input threads feed, so the rest of the daemon stays synchronous.

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

The hook writes the event to the stream socket `$XDG_RUNTIME_DIR/klaudo/listen.sock`
with `$TMUX`, `$TMUX_PANE` and its parent's pid, and exits. `PostToolUse` carries the
tool's whole output, which can pass the size limit of a datagram. Because of `exec`, that
parent is the session itself. The status line is the one place Claude Code reports the
context window and the plan's limits, so `klaudo status` forwards it and prints nothing.
Transcripts record neither, so the daemon logs the limits whenever a whole percentage or
a reset time changes, and the size of a session's context window whenever it changes.
A session of the Agent SDK or the VS Code extension draws no status line, so a program
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

`SessionStart` fires once the session is ready for input, after the trust dialog, which
is how a window opened from the chat knows when to type its first prompt.

`settings.json` also denies `AskUserQuestion`, which removes the tool from the model's
list, so a session asks in text that a reply from the chat answers. Its dialog takes
arrow keys and Enter, which pasted text cannot drive.

## Configuration

`BOT_TOKEN`, `CHAT_ID` and the optional `USER_ID`, `CHAT_PROJECTS` and `GUEST_IDS` come
from `$XDG_CONFIG_HOME/klaudo/env` alone, read with `dotenvy`, so an exported variable
reaches nothing. The daemon reads the file once as it starts, so a change to it applies
after `systemctl --user restart klaudo`. A switch such as `GUEST_PUSH` is on at `1` and off
at `0` or when absent, and any other value stops the daemon at startup. A private chat's id is its person's, so
`USER_ID` defaults to `CHAT_ID`, and a group, whose id is negative, needs it set. A
message Klaŭdo ignores is logged with its chat's and sender's ids, so `journalctl --user
-u klaudo` shows what to write here.

`CHAT_PROJECTS` lists absolute directories separated by `:`. A project inside one of
them posts its terminal-started turns to `CHAT_ID`.

`GUEST_IDS` lists user ids separated by `,`. Guests are answered in `CHAT_ID` alone, so a
private `CHAT_ID` with `GUEST_IDS` set stops the daemon at startup.

`GUEST_PUSH`, set to `1`, adds to the reminder that Claude may create commits and
push them for a guest. Telegram gives no email, so Claude asks the guest for the name
and email that a `Co-Authored-By` trailer on each of their commits credits.

`IDLE_HOURS`, a whole number defaulting to 6, is how long a window the daemon opened
stays open while its session idles.

`ATTACH_COMMAND` is the path of the program `/attach` runs, which has 10 seconds to
finish, and without it `/attach` stays out of the command menu. For a phone running
Termux whose `sshd` this machine reaches as `phone`, and which reaches this machine as
`pc`, the program asks Termux to open a session running ssh back:

```sh
#!/bin/sh
exec ssh phone am startservice --user 0 -n com.termux/com.termux.app.RunCommandService -a com.termux.RUN_COMMAND --es com.termux.RUN_COMMAND_PATH /data/data/com.termux/files/usr/bin/ssh --esa com.termux.RUN_COMMAND_ARGUMENTS "'-t,pc,tmux -S $2 -u new-session -t $1 \; set-option destroy-unattached on \; select-pane -t $1'" --ez com.termux.RUN_COMMAND_BACKGROUND false
```

The intent is Termux's
[`RUN_COMMAND`](https://github.com/termux/termux-app/wiki/RUN_COMMAND-Intent), which
Termux takes only with `allow-external-apps = true` in `~/.termux/termux.properties`,
and whose refusal shows as a notification on the phone while `am` still succeeds. The
tmux command shows the pane in a session of its own, which shares the windows of the
pane's session and is closed once detached, so a terminal already attached there keeps
its window. Over ssh no login shell sets a locale, so `-u` keeps tmux from drawing
non-ASCII as `_`. The phone's shell and this machine's both parse the arguments, hence
the `\;` inside single quotes. Both ssh connections need keys without a passphrase. Run
inside tmux as `attach "$TMUX_PANE" "$(tmux display -p '#{socket_path}')"`, the program
is tried without the chat.

`WEB_LISTEN` is the address the Mini App is served on, such as `127.0.0.1:8080`. The bot
needs a main Mini App, which BotFather sets up, and the daemon stops at start without
one. Telegram opens a Mini App only over HTTPS, so a reverse proxy forwards the URL
registered with BotFather to `WEB_LISTEN`, with
any path prefix stripped, since the page asks for its files relative to itself. The
page's scripts come to several hundred kilobytes, so the proxy should compress them.
Every file the page loads carries a hash of its content in its name, so the proxy may
cache them, but it must keep the daemon's `Cache-Control` on the page itself.

`TRACE_UPDATES`, set to `1`, logs every polled update as Telegram sent it.
`API_BASE` defaults to `https://api.telegram.org`, and the tests point it at their own
server. A local Bot API server (`--local`) lifts Telegram's 20 MB limit on the files a
bot can fetch, and gives the daemon the path of a file sent in the chat on the server's
own filesystem, so that path has to be readable on the daemon's machine.

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
was killed first. Where `/branch` or `/resume` switches the conversation of a process,
Claude Code ends the old session before the new one starts, and the daemon forgets the
old one only at that start, since a branch finds its origin and the `/branch` typed into
it by its pane.

A call Telegram rejects with a rate limit or its own failure is retried up to three
times, so a message can arrive late, or twice when the answer to an attempt was lost. A
rate limit naming a wait past 30 seconds fails the call at once.

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

`./check.sh` runs shellcheck, then installs, checks and builds `web/` with pnpm, then runs
formatting, clippy, the tests and a release build. The binary embeds `web/dist`, so
building it needs Node and pnpm. The lint levels live in `Cargo.toml`, so a bare
`cargo clippy` enforces the same set. `web/` checks its formatting with Prettier, lints
with oxlint running typescript-eslint's strict type-aware rules, and type-checks with
tsc. `pnpm -C web dev` serves the page with a stand-in for the daemon and Telegram, and
`/mock` there opens it as a file link would, `/mock?changes` as a link to changes, and
`dark` in either query in the dark theme.
`tests/turn.rs` drives the hook chain end to end in a throwaway runtime directory,
against a server answering as Telegram does and a `tmux` that records how it was called.

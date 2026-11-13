# Turns in the chat

A turn opens with its prompt, quoted and silent, and every later message of the turn
replies to it, so the chat reads as a thread per turn. A prompt Klaŭdo typed is already
in the chat as the message that asked for it, and its turn threads under that one. A
prompt submitted in the terminal during a turn is posted at once and queued, and the next
turn takes the oldest queued message. Editing a queued message in the terminal tells no
hook, so the pairing is by position and can attach a turn to the wrong prompt.

Every message opens with the directory Claude Code files the session's transcript under,
then `session/prompt` shortened to eight characters each. That line is the address a
reply is routed by, so a restarted daemon still routes replies to messages it never
posted.

## Segments

A turn is a sequence of segments, each an assistant message's text and the run of tool
calls it goes on to make, under that text in the same message. A run that would push
the message past Telegram's limit goes on in a message of its own. The open segment is
shown in one message at the foot of the turn, rewritten as it grows, with a status line
of a word from Claude Code's vocabulary and the elapsed time. Text growing past what that
message holds leaves it as it last fitted, and the complete segment takes its place as a
markdown file. In a group it is rewritten at most every ten seconds, because Telegram
counts a rewrite against the twenty messages a minute a bot may send there.

Telegram's message drafts would do the open segment's job in one call, and Klaŭdo was
built on them first. A draft expires thirty seconds after its last frame, no method
retires it, and clients differ on what they do when the real message arrives beside it,
from a clean transition to a duplicate to a crash. A `date_time` entity would keep the
status line's time current without rewrites, but clients show one inside a rich message
as its fallback text.

Claude Code fires `MessageDisplay` for the words introducing a tool call about a second
after that call's `PreToolUse`, and `PreToolUse` names no message, so the hooks leave the
two unordered. A flush or a tool outcome arriving after its call was filed is written
into the message its segment became, which is why a turn keeps its segments until it
ends.

While a turn runs, the bot shows as typing in the turn's topic, and the chat list shows
that too, so a topic with a turn running stands out without being opened. A session
waiting on a dialog shows none.

The last segment keeps no message of its own: the answer closing the turn carries its
text, and the message showing it is taken back once the answer is posted. A turn that
ends on a run keeps that last message, and the answer repeats its text. Only the answer
and a `Notification` make a sound, and only the answer carries the `#claude` tag.

`/compact` runs no turn, so the compaction is the answer to it, tagged
`#claude #compact`. A compaction Claude Code starts itself is a silent `#compact` message
in its turn's thread. Both quote the summary and the reasoning ahead of it, folded.
The transcript keeps only the summary, so the hook also appends each compaction's whole
output, with its time in Unix milliseconds and its trigger, as a line of JSON to
`$XDG_STATE_HOME/klaudo/compactions/<session_id>.jsonl`.

## Tool calls

A run of tool calls is a line per call:

● **Bash**  run the tests **4s**
⎿ `cargo test`
⎿ test result: ok. 58 passed
× **Bash**  lint everything **2s**
⎿ `cargo clippy`
⎿ Exit code 1
○ [Explore] **Grep**  `fn seal`

A finished call sums up its output in a line, as Claude Code's terminal does: the lines
a file read or an edit changed, or the files a search matched. A command shows the last
five lines it printed, and a failure the first five lines of its error, while they stay
in view as the next paragraph describes. An MCP tool follows no schema Klaŭdo could read
a subject or a summary from, and its arguments and output are usually what a reader
opens the call for, so its call shows both nearly whole.

While a run is the open segment, the calls from the first one still running to the end
stay in view, or else the latest call alone, since they are what a reader follows a turn
by, and their commands and summaries read more than twice as far as the others', which
show one line of what a command printed or an error said. The calls ahead of them, and
every call of a run the turn has moved on from, fold into one expandable quotation once
everything they show together runs past 120 characters, three lines of a phone's screen.

## Files from a session

`klaudo send <file>...` posts the files as documents in the thread of the turn that ran
it, up to ten per album, the first captioned with the head so a reply to it reaches the
session. Claude Code puts `CLAUDE_CODE_SESSION_ID` in the environment of every command
it runs, which is how the command finds its session. Run elsewhere, it sends to the chat
of the directory it runs in. The command asks the daemon where the files go and uploads
them itself, so its exit status says whether every file arrived.

## Rich messages

A message is posted with `sendRichMessage` and rewritten with `editMessageText`, both
carrying the body in `rich_message.markdown`. Bot API 10.1 added the method in June
2026, and its markdown is a dialect of its own, documented at
<https://core.telegram.org/bots/api#rich-message-formatting-options>. Claude Code answers
in CommonMark, which Klaŭdo posts with escapes only where the dialect reads it
differently, and Klaŭdo escapes the markdown in every other sentence a person or a tool
wrote.

A body too long for a rich message goes whole as `message.md`, a document captioned with
its title, so a reply to it reaches the session as a reply to any message does. Clients
render a markdown file in their in-app browser, where the desktop client shows HTML tags
as their source, so the folded quotations stay within a rich message. The body stays one
file because splitting it over rich messages would need a parser of the dialect to find
where no construct is left open, as a `<details>` holding blank lines is, and a code
block, whose text Telegram does not parse, scrolls sideways on every line.

A call Telegram rejects with a rate limit or its own failure is retried up to three
times, so a message can arrive late, or twice when the answer to an attempt was lost. A
rate limit naming a wait past 30 seconds fails the call at once.

## Usage figures

`/usage` answers with the plan's limits and the context of the session a message would
reach, from what the sessions' status lines last reported. Claude Code draws `/usage` as
a dialog that takes every key until dismissed, which is why the daemon answers it. The
message showing a running turn and the answer closing it end with the same figures, as
in `45.6k/1m 99% · $1.23 · 1% 3h30m · 56% 2d14h`, where `99%` is the share of the turn's
first call that Claude Code's transcript shows read from the prompt cache. Later calls of a
turn follow within seconds and nearly always hit the cache, while the first one misses it
once the cache has expired since the turn before.

`$1.23` is what the session's calls, its subagents' included, would cost at the API's
list prices, except the call that compacts the conversation, which the transcript does
not record. The daemon reads a transcript from where it ended when the daemon first
heard of the session, since the transcript of a long conversation runs to tens of
megabytes, so the cost starts over when the daemon restarts or the session is resumed.
Anthropic does not publish how a plan's limits are spent, so the figure only compares
turns and sessions with each other. The prices are written in the code, since the few
models Claude Code runs change seldom, and a session that called a model missing from
them shows no cost from then on. A session in the VS Code extension draws no status
line, so its context shows as the tokens alone, which the transcript records of each
call.

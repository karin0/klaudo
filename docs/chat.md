# Messages from the chat

A reply to any message from a turn is typed into that session's terminal as a prompt. A
message replying to nothing goes to the most recent session in the chat and topic it was
sent in, which is resumed first when it has exited. A session is as recent as the last
message of its conversation, its own or one typed into it, and the menus order sessions
and projects the same way. The last event a session reported only starts its idle time,
since a session reports events the chat never shows. A session is in its project's chat,
in the topic its last message went to, as [topics.md](topics.md) describes.

A turn a message started is posted where the message came from. A turn started in the
terminal goes to `CHAT_ID` when its project is in `CHAT_PROJECTS`, and to the private
chat otherwise. In a group, a bot in Telegram's default privacy mode receives only
commands and replies to its own messages, so an unaddressed message reaches Klaŭdo only
once privacy mode is off in BotFather's `/setprivacy` or the bot is an admin. A channel
is unsupported, since a post there carries no sender to check.

## Typing

Claude Code's own local messaging socket delivers text to a running session too, but
labels it as coming from another Claude session, to be treated as a peer's request and
never as the user's approval. That guardrail against permission laundering is why Klaŭdo
types through `send-keys`, the path a person's own typing takes.

The text keeps its newlines, quotes and non-ASCII. Telegram keeps the address of a link
and the marks of code apart from a message's text, so the daemon writes links, code and
code blocks back into the text as markdown. A message whose text reached an input box
gets a 👀 reaction once Claude Code reports the prompt.

A photo or a file sent in the chat is saved under `$XDG_STATE_HOME/klaudo/files/`, and
the prompt is its caption followed by the path of each file on a line of its own, in a
code span, since Claude Code turns a paste of one image's path alone into an attachment
and reports the prompt as `[Image #N]`. Claude opens an image it names with its Read
tool. The files of an album are gathered into one prompt. The files are kept as long as
the transcripts whose prompts name them. A message carrying a file is never one of the
daemon's commands.

A dialog in the terminal drops pasted text and takes the Enter after it as its default
choice, which approves a permission prompt's tool call. So while a session waits on a
dialog, from its `Notification` to the next event it reports, a message to it is typed
nothing and answered with `/attach` and `/stop`, which reach the dialog. A dialog
cancelled by Escape in the terminal reports nothing, so the session takes no message
from the chat until it next reports. `settings.json` denies `AskUserQuestion`, whose
dialog takes arrow keys and Enter, which pasted text cannot drive, so a session asks in
text that a reply answers.

A message to a running session outside tmux, such as one in an editor, has no terminal
to be typed into, so the daemon answers it with a button. Pressed by the message's
sender, the button opens a window running `claude --resume --fork-session`, which copies
the conversation into a new session and leaves the original running, and types the
message there once the branch is ready. The branch stays in the original's topic, and
works on the same files as the original.

`/stop` sends Escape to the pane of the session a message would reach while it runs a
turn, after the check a reply goes through. The turn ends with a silent `#interrupted`
message once it stops, and the thread says so when it did not. A `/stop` sent before the
last one is settled sends nothing. A background shell survives the interrupt and starts
a turn of its own once it exits. A button on the message showing a turn would be easy to
press by mistake, so stopping takes a command.

## Windows

A reply to a session that has exited opens a window running `claude --resume` in the
directory it ran in and types the reply once it is ready. The daemon remembers the
directories of the last thousand exited sessions, across restarts and reboots.

`/new <directory>` posts an anchor, and replying to it opens a window running `claude`
there with the reply as its first prompt. `/new` alone offers a menu of the chat's
projects, led by the project of the session the message would reach. A directory given
with no `~` or `/` in front is the most recent project of the chat with that name, or
else the path from home. `/clear` posts the anchor of the project of the session the
message would reach, which is the first choice `/new` alone offers, without the menu.
`tmux attach -t klaudo` reaches a conversation that began on the phone. A window there
whose session has been unheard from for `IDLE_HOURS` between turns is closed, to free its
memory, unless a process the session started is still running, and a reply resumes the
session like any other that exited.

An anchor carries `ForceReply`, which Telegram attaches only to a message being sent, so
an anchor is always a message of its own. Telegram refuses to edit a message carrying
`ForceReply`, so an anchor takes no button back to the projects. A client reopens the
reply box of a topic's latest anchor whenever the topic is entered, until a later message
of the bot there sets another reply markup, and then a message typed there goes to that
anchor's session. `/cancel` posts a message removing the markup, and so does a reply to
the anchor of `/new` or `/clear` once its window opens, since every message that anchor's
box catches would open another conversation. The box of another anchor sends a message
to the session just picked, so it stays until `/cancel`. The message removing the markup
has to stay, since deleting it brings the anchor's box back.

`/attach` shows the terminal of the session a message would reach on another device,
for the dialogs a reply cannot answer, such as the `/rewind` picker. It runs the program
`ATTACH_COMMAND` names with the session's tmux pane and the socket of its tmux server as
its two arguments, and marks the message with 👀 once the program succeeds. A session
that has exited is resumed in a window first, which the terminal then shows starting.
A window the daemon opened has no session to reach until its `SessionStart`, which
Claude Code reports only after dialogs such as the one asking whether a folder is
trusted. So a window whose session has not started 5 seconds after it opened, well past
the second Claude Code takes to start, shows its screen once to the place that asked for
it, with an `Attach` button that runs the same program with the window's pane.
For a phone running Termux whose `sshd` this machine reaches as `phone`, and which
reaches this machine as `pc`, the program asks Termux to open a session running ssh back:

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

`/issues` runs the program `ISSUES_COMMAND` names in the directory of the session a
message would reach, since a project may keep its issues somewhere its directory alone
does not tell. Without an argument the program prints the project's issues, and with one,
the text after `/issues` with its links and code written back as markdown, it files the
text as an issue, its first line the title. The daemon posts what the program prints as
markdown under the session's head, so a reply to the list reaches the session, and a
Telegram message holds at most 4096 characters, so the program should keep the list
short. With GitHub's `gh`:

```sh
#!/bin/sh
[ $# -eq 0 ] && exec gh issue list --limit 30 --json number,title,url --jq '.[] | "- [#\(.number)](\(.url)) \(.title)"'
exec gh issue create --title "$(printf '%s\n' "$1" | head -n 1)" --body "$(printf '%s\n' "$1" | tail -n +2)"
```

## Slash commands

Of Claude Code's own commands, `/compact`, `/branch`, and `/model`, `/effort` and
`/rename` with an argument go to a session like any other message, since they run without
a dialog. Any other message starting with a slash is typed with its first word in
backticks, so Claude reads it as text and invokes a skill it names through its Skill
tool, while a mistyped command, or one opening a dialog such as `/model` alone, opens
nothing in the terminal. A command Claude Code adds later reaches Claude as text until
the list of Claude Code's commands in `src/listen/chat.rs` includes it. The daemon's own
commands other than `/new`, `/diff`, `/issues` and `/branch @` take no argument, so one
followed by text, such as `/resume the design`, goes to a session the same way. The
daemon lists these commands in the command menu of each person it answers, shown to that
person alone.

## Guests

Only `USER_ID` is answered, in `CHAT_ID` or in that person's private chat with the bot,
and the guests in `GUEST_IDS`, in `CHAT_ID` alone. A guest reaches only the sessions and
directories inside `CHAT_PROJECTS`, whichever message, anchor, menu or button names them,
and is answered `that project is not open to guests` otherwise. Whoever can type into a
terminal can do as much as its shell, so the grant itself, who and which projects, is the
boundary. Within it, the daemon types each guest's text inside `<guest>`, behind a
`<system-reminder>` that names the guest, says how to address them, and limits Claude to
routine development in the session's directory, away from the files outside it other than
the toolchain's and the files the guest sent with the prompt, the system's state and the
local network. Each `@` a guest writes reaches the terminal as `&#64;`, since Claude Code
would attach the file it names before Claude reads the reminder. A guest's `/compact` and
`/branch` are typed as they were sent, since compacting and branching are routine work.
Either with arguments follows the reminder, since instructions to `/compact` would write
the summary every later turn reads, and `/branch @` is refused to them. Their other slash
commands follow the reminder, so Claude Code reads them as text, which keeps `/model`,
`/permissions` and the like out of a guest's reach. `/attach` would hand over the
terminal itself, and `/issues` runs a program with the daemon's own access, so a guest's
menu leaves them out and the daemon refuses them to guests.

`GUEST_PUSH`, set to `1`, adds to the reminder that Claude may create commits and
push them for a guest. Telegram gives no email, so Claude asks the guest for the name
and email that a `Co-Authored-By` trailer on each of their commits credits.

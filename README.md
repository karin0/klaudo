# Klaŭdo

Posts a Claude Code session's turns to a Telegram private chat or group and types the
replies you send there into the session's terminal, so a long turn can be left alone,
followed on the phone, and answered from there.

Klaŭdo is Esperanto for Claude, spelled `klaudo` in commands and paths.

## Installation

`./check.sh` builds `target/release/klaudo`, which needs a Rust toolchain, Node and pnpm.

```sh
sudo ln -s "$PWD/target/release/klaudo" /usr/local/bin/klaudo
mkdir -p ~/.config/klaudo && ln -s <secrets file> ~/.config/klaudo/env
systemctl --user enable --now "$PWD/klaudo.service"
```

Merge `settings.json` into `~/.claude/settings.json`. It takes over the status line and
denies `AskUserQuestion`, so a session asks its questions in text that a reply answers.
After a rebuild, restart the daemon with `systemctl --user restart klaudo`.

The daemon runs `claude` in windows of the tmux session `klaudo`, through tmux's
`default-shell` as a non-interactive shell, so `claude` has to be on the `PATH` of such a
shell. The native installer's `~/.local/bin` is often added only by an interactive
shell's startup file. A restart of the daemon kills a tmux server that the daemon
started, with every session it holds, so start the server under a unit of its own
ordered before the daemon.

## Configuration

The daemon reads these variables from `$XDG_CONFIG_HOME/klaudo/env`, and a change applies
after a restart. A switch is on at `1` and off at `0` or
when absent. A message Klaŭdo ignores is logged with its chat's and sender's ids, so
`journalctl --user -u klaudo` shows what to write here.

| Variable | Meaning |
| --- | --- |
| `BOT_TOKEN` | The bot's token. |
| `CHAT_ID` | The private chat or group turns go to. |
| `USER_ID` | The one person answered, defaulting to `CHAT_ID`, so a group needs it. |
| `CHAT_PROJECTS` | Absolute directories separated by `:`, whose terminal-started turns go to `CHAT_ID` and which guests may reach. Other turns go to the private chat. |
| `GUEST_IDS` | User ids separated by `,`, answered in a group `CHAT_ID` alone. |
| `GUEST_PUSH` | A switch letting Claude commit and push for a guest. |
| `IDLE_HOURS` | How long a window the daemon opened stays open while its session idles, 6 by default. |
| `ATTACH_COMMAND` | The program `/attach` runs, with 10 seconds to finish. Without it, `/attach` is left out of the command menu. |
| `WEB_LISTEN` | The address the review page is served on, such as `127.0.0.1:8080`. |
| `TRACE_UPDATES` | A switch logging every polled update. |

Whoever can type into a terminal can do as much as its shell, so who `GUEST_IDS` names
and which projects `CHAT_PROJECTS` lists is the boundary of what a guest can do.
[docs/chat.md](docs/chat.md) describes what the daemon adds to a guest's prompts, and an
`ATTACH_COMMAND` for a phone running Termux.

The review page needs a main Mini App, which BotFather sets up, and the daemon stops at
start without one. Telegram opens a Mini App only over HTTPS, so a reverse proxy forwards
the URL registered with BotFather to `WEB_LISTEN`, with any path prefix stripped. The
proxy should compress the page's scripts, may cache the files the page loads, and must
keep the daemon's `Cache-Control` on the page itself.

A local Bot API server (`--local`) lifts Telegram's 20 MB limit on the files a bot can
fetch, and gives the daemon the path of a file on the server's own filesystem, so that
path has to be readable on the daemon's machine.

## Usage

A turn shows in the chat as a thread under its prompt, with the text and tool calls in
progress rewritten at its foot and the answer closing it, as
[docs/turns.md](docs/turns.md) describes. A reply to any message of a turn is typed into
that session's terminal as a prompt. A message replying to nothing goes to the most
recent session in the chat and topic it was sent in. A session that has exited is
resumed first. A photo or a file sent in the chat reaches the session as its path.

| Command | Action |
| --- | --- |
| `/new [directory]` | Open a conversation in a directory, or pick one from a menu. |
| `/clear` | Open a conversation in the project of the one it replies to. |
| `/resume` | Resume a recent conversation and move it here. |
| `/stop` | Interrupt the running turn. |
| `/split` | Move the conversation into a topic of its own. |
| `/rewind` | Copy the conversation up to the end of the turn it replies to. |
| `/usage` | Show the plan's limits and the conversation's context. |
| `/diff [directory]` | Link the unpushed changes for review. |
| `/attach` | Open the conversation's terminal on another device. |
| `/cancel` | Close the reply box an earlier message keeps opening. |

`/compact`, `/branch`, and `/model`, `/effort` and `/rename` with an argument reach
Claude Code as typed. Any other message starting with a slash reaches Claude as text, so
it invokes a skill the message names. [docs/chat.md](docs/chat.md) and
[docs/topics.md](docs/topics.md) describe each command in full.

Claude can post files to the turn's thread with `klaudo send <file>...`, and a line in
`~/.claude/CLAUDE.md` pointing at `klaudo --help` is enough for Claude to learn it.

With `WEB_LISTEN` set, a file link in Claude's answer opens the file in the review page,
and a turn that changed tracked files ends with a link to its changes, where comments
on the lines go back to the session as a review, as [docs/review.md](docs/review.md)
describes.

## Caveats

While a session waits on a dialog in the terminal, such as a permission prompt, a
message to it is refused. `/attach` and `/stop` reach the dialog.

A session that opens a directory for the first time stops at the trust dialog and
reports what it shows to the chat. Answer it once locally and the directory stays
trusted.

In a group, the bot receives messages that are not replies to it only once privacy mode
is off in BotFather's `/setprivacy` or the bot is an admin, and it opens topics only with
the `can_manage_topics` right.

A private chat is split into topics once topic mode is on for the bot in BotFather, and
each project then gets a topic of its own. The daemon notices the change after a restart.
Topic mode slows the whole private chat, so a reply to `/new` can show several seconds
later than without it.

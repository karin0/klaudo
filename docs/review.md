# Review page

Claude Code links to the files it talks about, as in
`[listen.rs:588](src/listen.rs#L588)`. Once `WEB_LISTEN` is set, each inline link of a
message whose target has no scheme opens a page of every file the message links to in
the bot's main Mini App, which the daemon serves. The page opens at the link's own file
and at the lines a fragment `#L12` or `#L12-L20` names, or else a suffix `:12` or
`:12-20`, and tints the lines of every link. A link carries the files of its message.
When they are more than a link can carry, the daemon keeps their list under
`$XDG_STATE_HOME/klaudo/links/` for as long as it remembers the session, and the link
names the list. A relative path resolves in the session's directory.

A turn of a session in a git working tree that changed a tracked file there ends with a
link to the changes, as in `[Review changes: 3 commits, 5 files, +120 -30](https://t.me/<bot>?startapp=…)`.
The link counts tracked files alone, since a session usually commits the files it
creates, while a user may keep files of their own untracked. The link is in the answer's
text, since the daemon rewrites a message without its buttons, and an answer split over
several messages would have to carry a button on its last one.

Both links open one page, which shows a working tree since a commit: every changed file,
or the files a message links to. The daemon records the commit checked out as a turn
starts, and the page compares it with the working tree as it is when the page opens,
untracked files included, so an older link shows everything since its turn began. A
changed file shows each of its changes. A file a message links to shows the lines its
links name, and when the session's working tree holds it, each change since the commit
that a link names a line of, so the places a message points at stand apart from the
file's other changes. Both show 5 lines around each, where any other change shows too,
and collapse the rest, which a tap expands as the file is, without the marks of the
changes collapsed there. A link naming no lines shows its whole file. A menu on the page
lists the latest 30 commits of the first-parent history of `HEAD`, grouped by whether
the changes take them in, and choosing one moves the start to its parent, so the changes
then take in that commit, every later one, and the working tree. The menu can also move
the start to `HEAD`, which leaves the working tree alone. The page also lists the files,
each scrolling the page to itself on a tap, and a file the visitor cannot read shows why
in the list. The arrow in a file's header folds it.

`/diff` posts such a link under the head of the session a message would reach, for the
changes of its working tree since `HEAD` forked from its upstream, or since `HEAD` when
there is no upstream. `/diff <directory>` links the working tree holding that directory,
read as `/new` reads one, so the review of a repository the session changed outside its
own directory still goes to the session.

## Comments

On the page, a tap on a line number opens a comment there, and a tap on another in
the same file stretches it over the lines between, as a drag across line numbers does.
A comment may span removed lines, which the prompt names as git names a
file as of a commit, as in `0123456789ab:src/a.rs:3-4`, so Claude does not read them as
lines of the file as it is. `Suggest change` fills an editor with the lines to rewrite,
and only lines of the file as it is can be rewritten. The prompt marks the place of a
suggestion `(suggestion)`, so Claude judges it before applying it. The top of the page lists the
comments saved, each scrolling the page to its lines on a tap. `Send review` types every
comment into the session as one prompt, the way a reply from the chat is typed, so a
guest's review goes behind the reminder and a session waiting on a dialog refuses it.
`Add message` beside it opens a sheet for a message, which opens the prompt in place of
`Review:`, and `Send review` stays below the sheet to send it at once. The
daemon first posts the prompt quoted under the session's head where the session posts
between turns, and the turn it starts replies to that message, as a turn replies to the
message that asked for it.

## Access

Only Telegram can open the page, since the daemon checks the signature Telegram puts on
what it hands the page with its own key, so a leaked bot token cannot open it. The page
reaches as far as a reply could: the user reads any file the daemon can, and a guest only
the sessions and the files inside `CHAT_PROJECTS`, with symlinks resolved, and the
changes of a working tree whose top is inside them, so the page of a message's files
shows a guest no changes when that top is outside them. The page stops loading files six
hours after it was opened, until the link is opened again. A file past 1 MiB or not in
UTF-8 is refused.

## Build

The page is a React app in `web/`, which pnpm builds into `web/dist` for the binary to
embed, so the page always matches the daemon serving it. The server runs tokio and hyper
on a thread of its own, so the rest of the daemon stays synchronous.

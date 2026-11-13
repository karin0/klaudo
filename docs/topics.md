# Topics and conversations

A private chat is split into topics once topic mode is on for the bot in BotFather, and
a group is when it is a forum. A private chat in topic mode takes no message outside
every topic, since one sent there opens a topic of its own. A topic reaches only its own
sessions, so a message sent outside every topic by mistake reaches no session. The
daemon reads which chats are split into topics once as it starts, so turning topic mode
on or off applies after a restart. Topic mode slows the whole private chat. A reply to
`/new` shows several seconds later where topic mode is on than where it is off.

A session is in its project's chat, in the topic its last message went to. A session
that has not posted there yet takes the topic of the most recent session of its project,
so a conversation restarted in a project stays in its topic. In a chat split into
topics, a session new to the daemon that no message from the chat started, in a project
with no session standing in a topic, opens a topic named after the project's directory,
which the project's later sessions inherit. A session that stood outside every topic
moves the same way when the terminal or the Agent SDK resumes it, so a session resumed
for every turn leaves the place it held before the chat was split.

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

`/resume` goes straight to the sessions of the project the message would reach, with a
button back to the menu of projects, which it offers first when the message reaches no
session. The anchor it posts replies to the last message the session left, since a
private chat has no link to a single message. The anchor moves the session to where it
is posted, without a reply: a running turn goes on there under the anchor, leaving what
it already posted behind, and so does every later one. Prompts already queued in the
terminal stay where they were posted, since their turns reply to them.

## Rewinding

`/rewind` copies the conversation of the session a message would reach into a new
session, up to the end of the turn of the message it replies to. Claude Code's own
`/rewind` picker goes back to before the prompt picked, but a prompt sent from the chat
is the sender's own message, which carries no head to reply to, so a reply picks the
answer it was given and keeps it. A message that names no turn, such as one replying to
nothing, keeps the conversation up to the end of its latest compaction, which is also
what a reply to the compaction's summary keeps. The copy's head is posted where the
message was sent, over the first prompt left out, as the picker hands that prompt back,
so a reply resumes the copy there and the original stays as it was. A copy wanting a
topic of its own takes `/split`. Only the conversation goes back, so the copy works on
the files as they are.

Claude Code's `/rewind` picker takes only arrow keys, and `--resume-session-at`
truncates a resumed conversation only in print mode, so the daemon writes the
transcript of a rewound copy itself. The daemon keeps the transcript path each
session's hooks named, since a session that has exited reports none.

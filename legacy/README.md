# The shell implementation this replaced

Four hook scripts that lived in a private `bin` directory and reached Telegram through
`curl`. They are kept because they are the only record of the behaviour klaude was
measured against, not because anything still runs them.

`claude-notify` alone was the original: one `sendRichMessage` per turn, at `Stop`. The
other three added the streaming draft, and the split between `claude-stream` and
`claude-stream-send` exists because a single file that both returned fast and held a
`flock` for the turn confused shellcheck's scope analysis.

Measured on the machine klaude was written for, `claude-stream` cost 9.4 ms per
invocation, against 0.5 ms for a compiled binary that does the same handoff. The
`MessageDisplay` hook runs on every flush of streamed text and the terminal draws that
text only after the hook returns, which is what made the cost worth removing.

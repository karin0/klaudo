# shellcheck shell=bash
# Telegram plumbing shared by the Claude Code hooks beside this file.

. "$(dirname "${BASH_SOURCE[0]}")"/.env
: "$BOT_TOKEN" "$CHAT_ID"

stream_dir=${XDG_RUNTIME_DIR:-/tmp}/claude-stream

tg_title() {
  local cwd=$1 session=$2 took=${3-} branch
  branch=$(git -C "$cwd" branch --show-current 2>/dev/null || :)
  # The backticks are markdown, not a substitution.
  # shellcheck disable=SC2016
  printf '**%s** %s `%s`%s' "${cwd##*/}" "$branch" "${session:0:8}" "$took"
}

# Request body on stdin. A rejected call is reported on stderr and the caller carries on.
tg_post() {
  local resp
  resp=$(curl -sS --max-time 30 -X POST "https://api.telegram.org/bot$BOT_TOKEN/$1" \
    -H 'Content-Type: application/json' -d @- || :)
  [[ $resp == *'"ok":true'* ]] || echo "${0##*/}: $1: $resp" >&2
}

# A draft and the message that replaces it must not overlap in the chat, so the streamer
# is told to stop and is waited out before the final message goes.
stream_stop() {
  local state=$stream_dir/$1
  [ -e "$state/turn" ] || return 0
  : > "$state/done"
  flock -w 5 "$state/lock" true || :
}

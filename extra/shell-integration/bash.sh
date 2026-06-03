# rusty_term shell integration for bash — emits OSC 133 prompt marks so the
# terminal can jump between prompts (Ctrl+Shift+PageUp / Ctrl+Shift+PageDown).
#
#   Add to ~/.bashrc:   source /path/to/extra/shell-integration/bash.sh
#
# Marks emitted: A=prompt start, B=prompt end / command start,
# C=command output start, D;<exit>=command finished. rusty_term navigates on A;
# the rest are emitted for compatibility with other OSC 133 consumers.

# Only interactive bash, and only once.
if [ -z "$BASH_VERSION" ] || [[ $- != *i* ]] || [ -n "$__RT_SHELL_INTEGRATION" ]; then
  return 0 2>/dev/null || true
fi
__RT_SHELL_INTEGRATION=1

# D (just-finished command's exit status) — runs first in PROMPT_COMMAND, so $?
# is still the command's status. Also arms the next C.
__rt_precmd() {
  local st=$?
  printf '\033]133;D;%s\007' "$st"
  __rt_preexec_armed=1
}
case ";$PROMPT_COMMAND;" in
  *";__rt_precmd;"*) ;;
  ";;") PROMPT_COMMAND="__rt_precmd" ;;
  *) PROMPT_COMMAND="__rt_precmd;$PROMPT_COMMAND" ;;
esac

# Wrap PS1 with A (prompt start) … B (prompt end). The \[ \] markers keep the
# escapes zero-width for bash's line-length accounting.
case "$PS1" in
  *'133;A'*) ;;
  *) PS1='\[\033]133;A\007\]'"$PS1"'\[\033]133;B\007\]' ;;
esac

# C (command output start) — the DEBUG trap fires before each command; emit only
# for the first real command after a prompt (not for PROMPT_COMMAND or completion).
__rt_preexec_armed=0
__rt_preexec() {
  [ -n "$COMP_LINE" ] && return
  [ "$BASH_COMMAND" = "$PROMPT_COMMAND" ] && return
  [ "$__rt_preexec_armed" = 1 ] || return
  __rt_preexec_armed=0
  printf '\033]133;C\007'
}
trap '__rt_preexec' DEBUG

# rusty_term shell integration for zsh — emits OSC 133 prompt marks so the
# terminal can jump between prompts (Ctrl+Shift+PageUp / Ctrl+Shift+PageDown).
#
#   Add to ~/.zshrc:   source /path/to/extra/shell-integration/zsh.sh
#
# Marks: A=prompt start, B=prompt end, C=command output start,
# D;<exit>=command finished. rusty_term navigates on A.

# Interactive only, and only once.
[[ -o interactive ]] || return 0
[[ -n "$__RT_SHELL_INTEGRATION" ]] && return 0
__RT_SHELL_INTEGRATION=1

autoload -Uz add-zsh-hook

# precmd runs just before the prompt is drawn: report the previous command's
# exit status (D), then open the new prompt (A).
__rt_precmd() {
  local st=$?
  print -n "\e]133;D;${st}\a"
  print -n "\e]133;A\a"
}

# preexec runs after the user submits a line, before it executes (C).
__rt_preexec() {
  print -n "\e]133;C\a"
}

add-zsh-hook precmd __rt_precmd
add-zsh-hook preexec __rt_preexec

# Close the prompt (B) at the very end of PROMPT. %{ %} keep it zero-width.
if [[ "$PROMPT" != *'133;B'* ]]; then
  PROMPT="${PROMPT}%{"$'\e]133;B\a'"%}"
fi

# rusty_term shell integration for PowerShell — emits OSC 133 prompt marks so
# the terminal can jump between prompts (Ctrl+Shift+PageUp / Ctrl+Shift+PageDown).
#
#   Add to $PROFILE:   . /path/to/extra/shell-integration/pwsh.ps1
#
# Marks: A=prompt start, B=prompt end, D;<exit>=command finished. rusty_term
# navigates on A. (PowerShell has no portable pre-execution hook, so C is
# omitted.)

if ($Global:__RtShellIntegration) { return }
$Global:__RtShellIntegration = $true

# Preserve the user's existing prompt so we only wrap it.
$Global:__RtOriginalPrompt = $function:prompt

function global:prompt {
    $exit = $LASTEXITCODE
    if ($null -eq $exit) { $exit = 0 }
    $esc = [char]27
    $bel = [char]7
    # D (previous command's exit status) then A (prompt start).
    [Console]::Write("$esc]133;D;$exit$bel$esc]133;A$bel")
    $rendered = & $Global:__RtOriginalPrompt
    # B (prompt end) after the user's prompt text.
    [Console]::Write("$esc]133;B$bel")
    $rendered
}

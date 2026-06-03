# rusty_term shell integration for fish — emits OSC 133 prompt marks so the
# terminal can jump between prompts (Ctrl+Shift+PageUp / Ctrl+Shift+PageDown).
#
#   Add to ~/.config/fish/config.fish:
#       source /path/to/extra/shell-integration/fish.fish
#
# Marks: A=prompt start, B=prompt end, C=command output start,
# D;<exit>=command finished. rusty_term navigates on A.

if status is-interactive
    and not set -q __RT_SHELL_INTEGRATION
        set -g __RT_SHELL_INTEGRATION 1

        # C — a command line was submitted and is about to run.
        function __rt_preexec --on-event fish_preexec
            printf '\e]133;C\a'
        end

        # D — the command finished; report its exit status.
        function __rt_postexec --on-event fish_postexec
            printf '\e]133;D;%s\a' $status
        end

        # Wrap the existing fish_prompt with A (start) and B (end). Copy the
        # current definition once, then redefine fish_prompt to bracket it.
        if not functions -q __rt_orig_fish_prompt
            functions -c fish_prompt __rt_orig_fish_prompt
        end
        function fish_prompt
            printf '\e]133;A\a'
            __rt_orig_fish_prompt
            printf '\e]133;B\a'
        end
end

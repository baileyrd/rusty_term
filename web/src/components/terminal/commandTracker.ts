/**
 * OSC 133 command tracker: turns a live terminal's shell-integration marks
 * into structured command events, so the Nebula command cards can be fed by
 * a real session instead of demo data.
 *
 * The protocol is the FinalTerm/VS Code/rusty_term one the repo's native
 * front-end already consumes for its gutter marks and command dock:
 *
 * - `OSC 133;A ST` — prompt start
 * - `OSC 133;B ST` — prompt end (the command the user types follows)
 * - `OSC 133;C ST` — command output begins (pre-exec)
 * - `OSC 133;D;<code> ST` — command finished, with its exit code
 *
 * The shell must emit these (see web/README.md for a bash snippet). Without
 * them the tracker simply never fires and the UI keeps its demo cards —
 * semantic features are additive, matching the native renderer's rule.
 *
 * All buffer reads go through xterm's public API (markers survive scrollback
 * trimming; `translateToString` reads rendered rows), so the tracker never
 * touches the data stream itself — it observes the same bytes the terminal
 * renders.
 */

import type { IDisposable, IMarker, Terminal } from '@xterm/xterm';

export type CommandEvent =
  | { type: 'start'; command: string; startedAt: number }
  | { type: 'finish'; exit: number | null; output: string[]; finishedAt: number };

/** Most output lines captured per command card; the terminal keeps the rest. */
const OUTPUT_LINES_MAX = 30;

export function attachCommandTracker(
  term: Terminal,
  onEvent: (event: CommandEvent) => void,
): IDisposable {
  let promptEnd: { marker: IMarker; x: number } | null = null;
  let output: IMarker | null = null;
  let running = false;

  const buffer = () => term.buffer.active;
  const cursorAbs = () => buffer().baseY + buffer().cursorY;
  const lineText = (y: number) => buffer().getLine(y)?.translateToString(true) ?? '';

  /** The command the user typed: the prompt line, minus the prompt itself. */
  function commandText(): string {
    if (promptEnd && !promptEnd.marker.isDisposed) {
      return lineText(promptEnd.marker.line).slice(promptEnd.x).trim();
    }
    // No B mark (sparser integrations): best effort, the line above the
    // output start — same fallback the native command dock uses.
    return lineText(Math.max(0, cursorAbs() - 1)).trim();
  }

  /** Output rows between the C mark and the cursor (the D position). */
  function outputLines(): string[] {
    if (!output || output.isDisposed) return [];
    const lines: string[] = [];
    const end = Math.min(cursorAbs(), output.line + OUTPUT_LINES_MAX);
    for (let y = output.line; y < end; y++) {
      lines.push(lineText(y));
    }
    while (lines.length > 0 && lines[lines.length - 1] === '') lines.pop();
    return lines;
  }

  const handler = term.parser.registerOscHandler(133, (data: string) => {
    const [mark, arg] = data.split(';');
    switch (mark) {
      case 'B':
        promptEnd?.marker.dispose();
        promptEnd = { marker: term.registerMarker(0), x: buffer().cursorX };
        break;
      case 'C': {
        const command = commandText();
        output?.dispose();
        output = term.registerMarker(0);
        running = true;
        onEvent({ type: 'start', command, startedAt: Date.now() });
        break;
      }
      case 'D': {
        if (!running) break; // a D with no C (first prompt): nothing to close
        running = false;
        const exit = arg === undefined || arg === '' ? null : Number.parseInt(arg, 10);
        onEvent({
          type: 'finish',
          exit: Number.isNaN(exit as number) ? null : exit,
          output: outputLines(),
          finishedAt: Date.now(),
        });
        break;
      }
      default:
        break; // A (prompt start) needs no card-side action
    }
    return true;
  });

  return {
    dispose() {
      handler.dispose();
      promptEnd?.marker.dispose();
      output?.dispose();
    },
  };
}

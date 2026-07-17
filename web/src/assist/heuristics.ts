/**
 * Local assist heuristics: the brains behind the AI orb, computed entirely
 * client-side from the session's real command cards — no network, no model,
 * no fabricated intelligence. The design docs' orb promises "contextual
 * summaries and next-command suggestions"; this provider delivers the
 * honest subset that pattern rules can, and the `AssistProvider` shape
 * leaves a socket for a real LLM provider later (the architecture doc lists
 * assistant providers as an extension surface).
 */

import type { CommandCardProps } from '../components/terminal/types';

export interface AssistInsight {
  id: string;
  kind: 'summary' | 'failure' | 'tip';
  title: string;
  body: string;
  /** A command the user can run straight from the panel, when one is safe to suggest. */
  suggestedCommand?: string;
}

/** The pluggable shape: today's local rules, tomorrow's LLM. */
export interface AssistProvider {
  analyze(commands: CommandCardProps[]): AssistInsight[];
}

/** What a failure's output tells us, by unglamorous pattern matching. */
function diagnoseFailure(card: CommandCardProps): AssistInsight {
  const text = card.output.join('\n');
  const base = {
    id: `failure-${card.id ?? card.command}`,
    kind: 'failure' as const,
    title: `Failed: ${card.command}`,
  };
  if (/permission denied|operation not permitted/i.test(text)) {
    const sudoed = card.command.startsWith('sudo ');
    return {
      ...base,
      body: 'The command was refused for lack of permissions.',
      suggestedCommand: sudoed ? undefined : `sudo ${card.command}`,
    };
  }
  if (/command not found|not recognized as an internal/i.test(text)) {
    const name = card.command.split(/\s+/)[0];
    return {
      ...base,
      body: `\`${name}\` isn't on the PATH — a typo, or not installed.`,
      suggestedCommand: `command -v ${name}`,
    };
  }
  if (/no such file or directory/i.test(text)) {
    return {
      ...base,
      body: 'A path in the command does not exist. Check the spelling, or list the parent directory.',
      suggestedCommand: card.command,
    };
  }
  const meta = card.meta ? ` (${card.meta})` : '';
  return {
    ...base,
    body: `The command exited non-zero${meta}. Re-run it to see the failure again, or scroll the terminal for full output.`,
    suggestedCommand: card.command,
  };
}

/** Today's provider: a session summary, recent-failure diagnoses, a repeat-failure tip. */
export const localHeuristics: AssistProvider = {
  analyze(commands: CommandCardProps[]): AssistInsight[] {
    const finished = commands.filter((c) => c.status === 'success' || c.status === 'error');
    if (finished.length === 0) {
      return [
        {
          id: 'empty',
          kind: 'summary',
          title: 'No commands yet',
          body: 'Run something in the terminal (or the input line) and insights about failures and patterns will appear here.',
        },
      ];
    }
    const insights: AssistInsight[] = [];
    const failures = finished.filter((c) => c.status === 'error');
    const durations = finished
      .filter((c) => c.startedAt !== undefined && c.finishedAt !== undefined)
      .map((c) => (c.finishedAt as number) - (c.startedAt as number));
    const total = durations.reduce((a, b) => a + b, 0);
    insights.push({
      id: 'summary',
      kind: 'summary',
      title: 'Session',
      body: `${finished.length} command${finished.length === 1 ? '' : 's'} run, ${failures.length} failed` +
        (durations.length > 0 ? `, ${(total / 1000).toFixed(1)}s total runtime.` : '.'),
    });
    // The most recent failures (newest first), each diagnosed.
    for (const card of failures.slice(-2).reverse()) {
      insights.push(diagnoseFailure(card));
    }
    // The same command failing twice in a row deserves a nudge.
    const last = finished[finished.length - 1];
    const prev = finished[finished.length - 2];
    if (
      last?.status === 'error' &&
      prev?.status === 'error' &&
      last.command === prev.command
    ) {
      insights.push({
        id: 'repeat',
        kind: 'tip',
        title: 'Repeating failure',
        body: `\`${last.command}\` has now failed twice in a row — the retry isn't changing the outcome; the input probably needs to.`,
      });
    }
    return insights;
  },
};

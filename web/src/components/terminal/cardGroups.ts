/**
 * Grouping for the command-card stream: consecutive cards form a "burst"
 * until the shell sits idle for a while, then the next command starts a new
 * group. Groups render with a collapsible header (time range, count,
 * failures) so an old day's worth of history folds down to a few lines.
 */

import type { CommandCardProps } from './types';

/** Idle time between commands that starts a new group. */
export const GROUP_GAP_MS = 5 * 60_000;

export interface CardGroup {
  /** Stable id: the first card's id (or its index when the card has none). */
  id: string;
  cards: CommandCardProps[];
  startedAt?: number;
  finishedAt?: number;
  failures: number;
}

export function groupCards(commands: CommandCardProps[]): CardGroup[] {
  const groups: CardGroup[] = [];
  let prevEnd: number | undefined;
  for (const [i, card] of commands.entries()) {
    const gap =
      card.startedAt !== undefined && prevEnd !== undefined ? card.startedAt - prevEnd : 0;
    if (groups.length === 0 || gap > GROUP_GAP_MS) {
      groups.push({
        id: card.id ?? `group-${i}`,
        cards: [],
        startedAt: card.startedAt,
        failures: 0,
      });
    }
    const group = groups[groups.length - 1];
    group.cards.push(card);
    if (card.status === 'error') group.failures++;
    if (card.finishedAt !== undefined) group.finishedAt = card.finishedAt;
    // A card with no timestamps extends the current group without moving
    // the idle clock.
    prevEnd = card.finishedAt ?? card.startedAt ?? prevEnd;
  }
  return groups;
}

function clock(ts: number): string {
  return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

/** Header label: "14:02 – 14:31 · 12 commands · 2 failed". */
export function groupLabel(group: CardGroup): string {
  const parts: string[] = [];
  if (group.startedAt !== undefined) {
    const start = clock(group.startedAt);
    const end = group.finishedAt !== undefined ? clock(group.finishedAt) : null;
    parts.push(end !== null && end !== start ? `${start} – ${end}` : start);
  }
  parts.push(`${group.cards.length} command${group.cards.length === 1 ? '' : 's'}`);
  if (group.failures > 0) {
    parts.push(`${group.failures} failed`);
  }
  return parts.join(' · ');
}

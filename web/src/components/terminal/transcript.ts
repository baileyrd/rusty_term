/**
 * Session transcript export: turn a tab's card history into a portable
 * file. Markdown reads like a lab notebook (grouped into the same bursts
 * the stream shows, output fenced); JSON is the raw cards for tooling.
 */

import { groupCards, groupLabel } from './cardGroups';
import type { CommandCardProps } from './types';

const STATUS_GLYPH: Record<CommandCardProps['status'], string> = {
  idle: '·',
  running: '…',
  success: '✔',
  error: '✘',
};

export function buildMarkdownTranscript(title: string, cards: CommandCardProps[]): string {
  const lines: string[] = [
    `# rusty_term transcript — ${title}`,
    '',
    `Exported ${new Date().toISOString()} · ${cards.length} command${cards.length === 1 ? '' : 's'}`,
    '',
  ];
  for (const group of groupCards(cards)) {
    lines.push(`## ${groupLabel(group)}`, '');
    for (const card of group.cards) {
      const meta = [STATUS_GLYPH[card.status], card.meta].filter(Boolean).join(' ');
      lines.push(`### \`${card.command}\``, '');
      if (meta.length > 0) lines.push(meta, '');
      if (card.output.length > 0) {
        lines.push('```text', ...card.output, '```', '');
      }
    }
  }
  return lines.join('\n');
}

export function buildJsonTranscript(title: string, cards: CommandCardProps[]): string {
  return JSON.stringify(
    { title, exportedAt: new Date().toISOString(), commands: cards },
    null,
    2,
  );
}

/** Slug for a download filename: "session 2" → "session-2". */
function slug(title: string): string {
  return title.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '') || 'session';
}

export function transcriptFilename(title: string, format: 'md' | 'json'): string {
  const day = new Date().toISOString().slice(0, 10);
  return `rusty-term-${slug(title)}-${day}.${format}`;
}

/** Hand a text file to the browser's download machinery. */
export function downloadFile(name: string, mime: string, text: string): void {
  const url = URL.createObjectURL(new Blob([text], { type: mime }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}

/** The palette-facing entry: build and download in the chosen format. */
export function exportTranscript(
  title: string,
  cards: CommandCardProps[],
  format: 'md' | 'json',
): void {
  const text =
    format === 'md' ? buildMarkdownTranscript(title, cards) : buildJsonTranscript(title, cards);
  downloadFile(
    transcriptFilename(title, format),
    format === 'md' ? 'text/markdown' : 'application/json',
    text,
  );
}

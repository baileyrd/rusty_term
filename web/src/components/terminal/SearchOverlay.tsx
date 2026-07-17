import { useEffect, useMemo, useRef, useState } from 'react';
import { useOverlayEscape, useOverlayLifecycle } from './useOverlay';
import type { CommandCardProps } from './types';

/** One searchable session: a tab and its card history. */
export interface SearchSession {
  id: string;
  title: string;
  commands: CommandCardProps[];
}

/** A hit: which card matched, where, and the line it matched in. */
interface SearchHit {
  tabId: string;
  tabTitle: string;
  cardId: string;
  command: string;
  /** The matching line (the command itself, an output line, or the meta). */
  line: string;
  matchStart: number;
  matchLen: number;
}

export interface SearchOverlayProps {
  open: boolean;
  onClose: () => void;
  sessions: SearchSession[];
  /** Jump to a hit: switch to its tab and flash its card. */
  onJump?: (tabId: string, cardId: string) => void;
}

const MAX_HITS = 50;

/** Case-insensitive substring search over every card in every session. */
export function searchHistory(sessions: SearchSession[], query: string): SearchHit[] {
  const q = query.trim().toLowerCase();
  if (q.length === 0) return [];
  const hits: SearchHit[] = [];
  // Newest hits first: walk each session's cards backwards.
  for (const session of sessions) {
    for (let i = session.commands.length - 1; i >= 0; i--) {
      const card = session.commands[i];
      if (card.id === undefined) continue;
      const lines = [card.command, ...card.output, ...(card.meta ? [card.meta] : [])];
      for (const line of lines) {
        const at = line.toLowerCase().indexOf(q);
        if (at === -1) continue;
        hits.push({
          tabId: session.id,
          tabTitle: session.title,
          cardId: card.id,
          command: card.command,
          line,
          matchStart: at,
          matchLen: q.length,
        });
        break; // one hit per card
      }
      if (hits.length >= MAX_HITS) return hits;
    }
  }
  return hits;
}

function Highlighted({ hit }: { hit: SearchHit }) {
  const before = hit.line.slice(Math.max(0, hit.matchStart - 40), hit.matchStart);
  const match = hit.line.slice(hit.matchStart, hit.matchStart + hit.matchLen);
  const after = hit.line.slice(hit.matchStart + hit.matchLen, hit.matchStart + hit.matchLen + 80);
  return (
    <span className="truncate font-nebula-output text-xs text-nebula-text/60">
      {hit.matchStart > 40 && '…'}
      {before}
      <mark className="rounded-sm bg-nebula-accent2/30 px-0.5 text-nebula-accent2">{match}</mark>
      {after}
    </span>
  );
}

/**
 * Ctrl/Cmd+Shift+F: search the whole workspace's history — every tab's
 * command cards (commands, output, meta) — and jump to a hit: its tab is
 * activated and the card scrolled into view and flashed. Same overlay
 * conventions as the palette: arrows move, Enter jumps, Esc closes.
 */
export default function SearchOverlay({ open, onClose, sessions, onJump }: SearchOverlayProps) {
  const [query, setQuery] = useState('');
  const [cursor, setCursor] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  useOverlayLifecycle(open, {
    onOpen: () => requestAnimationFrame(() => inputRef.current?.focus()),
    onClose: () => {
      setQuery('');
      setCursor(0);
    },
  });
  useOverlayEscape(open, onClose);

  const hits = useMemo(
    () => (open ? searchHistory(sessions, query) : []),
    [open, sessions, query],
  );

  useEffect(() => {
    if (cursor >= hits.length) setCursor(Math.max(0, hits.length - 1));
  }, [hits.length, cursor]);

  useEffect(() => {
    listRef.current
      ?.querySelector('[data-active="true"]')
      ?.scrollIntoView({ block: 'nearest' });
  }, [cursor, hits]);

  if (!open) return null;

  const pick = (hit: SearchHit) => {
    onJump?.(hit.tabId, hit.cardId);
    onClose();
  };

  return (
    <div className="fixed inset-0 z-30 bg-black/50" onClick={onClose}>
      <div
        data-testid="search-overlay"
        role="dialog"
        aria-label="Search session history"
        onClick={(e) => e.stopPropagation()}
        className="mx-auto mt-[12vh] flex w-[38rem] max-w-[calc(100vw-2rem)] animate-nebula-fade-in flex-col overflow-hidden rounded-nebula-lg border border-white/10 bg-nebula-bg shadow-nebula-soft"
      >
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') {
              e.preventDefault();
              setCursor((c) => Math.min(c + 1, hits.length - 1));
            } else if (e.key === 'ArrowUp') {
              e.preventDefault();
              setCursor((c) => Math.max(c - 1, 0));
            } else if (e.key === 'Enter' && hits[cursor]) {
              e.preventDefault();
              pick(hits[cursor]);
            }
          }}
          placeholder="Search commands and output across all sessions…"
          aria-label="History search query"
          data-testid="search-input"
          className="border-b border-white/10 bg-nebula-surface px-4 py-3 font-nebula-command text-sm text-nebula-text placeholder:text-nebula-text/30 focus:outline-none"
        />
        <div ref={listRef} className="max-h-[46vh] overflow-y-auto p-1.5">
          {query.trim().length > 0 && hits.length === 0 && (
            <p className="px-3 py-4 font-nebula-meta text-xs text-nebula-text/40">
              No matches in any session.
            </p>
          )}
          {hits.map((hit, i) => (
            <button
              key={`${hit.cardId}-${i}`}
              type="button"
              data-testid="search-hit"
              data-tab={hit.tabId}
              data-active={i === cursor}
              onClick={() => pick(hit)}
              onMouseEnter={() => setCursor(i)}
              className={`flex w-full flex-col gap-0.5 rounded-nebula-sm px-3 py-2 text-left transition-colors duration-nebula-fast ease-nebula ${
                i === cursor ? 'bg-nebula-accent/15' : 'hover:bg-white/5'
              }`}
            >
              <span className="flex items-baseline gap-2">
                <span className="shrink-0 rounded-full border border-white/10 px-2 font-nebula-meta text-[10px] text-nebula-text/40">
                  {hit.tabTitle}
                </span>
                <span className="truncate font-nebula-command text-sm text-nebula-text">
                  {hit.command}
                </span>
              </span>
              <Highlighted hit={hit} />
            </button>
          ))}
        </div>
        <footer className="flex gap-3 border-t border-white/5 bg-nebula-surface px-4 py-1.5 font-nebula-meta text-[10px] text-nebula-text/30">
          <span>↑↓ navigate</span>
          <span>↵ jump to card</span>
          <span>esc close</span>
        </footer>
      </div>
    </div>
  );
}

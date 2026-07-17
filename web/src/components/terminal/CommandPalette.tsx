import { useEffect, useMemo, useRef, useState } from 'react';
import type { SnippetItem } from './types';

/** One row of the palette: what it shows and what happens on Enter. */
interface PaletteItem {
  id: string;
  /** Section label rendered above the first item of each group. */
  group: 'run' | 'snippets' | 'recent' | 'actions';
  title: string;
  detail?: string;
  action: () => void;
}

export interface CommandPaletteProps {
  open: boolean;
  onClose: () => void;
  snippets: SnippetItem[];
  recentCommands: string[];
  onRunCommand?: (command: string) => void;
  /** Open the assist sheet on the given tab. */
  onOpenAssist?: (tab: 'insights' | 'chat') => void;
}

const GROUP_LABEL: Record<PaletteItem['group'], string> = {
  run: 'run',
  snippets: 'pinned snippets',
  recent: 'recent commands',
  actions: 'actions',
};

/**
 * Case-insensitive subsequence match (the classic palette filter: "ctw"
 * hits "cargo test --workspace"). Returns a rank — lower is better, based
 * on how early and how tightly the query's characters land — or null.
 */
export function fuzzyRank(query: string, target: string): number | null {
  const q = query.toLowerCase();
  const t = target.toLowerCase();
  if (q.length === 0) return 0;
  let rank = 0;
  let pos = -1;
  for (const ch of q) {
    const next = t.indexOf(ch, pos + 1);
    if (next === -1) return null;
    rank += next - pos - 1; // gap size; contiguous matches cost nothing
    pos = next;
  }
  return rank;
}

/**
 * The Ctrl/Cmd+K command palette: a top-center overlay that fuzzy-filters
 * pinned snippets, recent commands, and shell actions, plus a raw "run
 * what I typed" entry so it doubles as a quick command launcher. Fully
 * keyboard-driven: arrows move, Enter runs, Esc closes.
 */
export default function CommandPalette({
  open,
  onClose,
  snippets,
  recentCommands,
  onRunCommand,
  onOpenAssist,
}: CommandPaletteProps) {
  const [query, setQuery] = useState('');
  const [cursor, setCursor] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (open) {
      setQuery('');
      setCursor(0);
      // The overlay mounts on this render; focus once it exists.
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);

  const items = useMemo<PaletteItem[]>(() => {
    if (!open) return [];
    const candidates: PaletteItem[] = [];
    if (onRunCommand && query.trim().length > 0) {
      const cmd = query.trim();
      candidates.push({
        id: 'raw-run',
        group: 'run',
        title: cmd,
        detail: 'run in terminal',
        action: () => onRunCommand(cmd),
      });
    }
    for (const s of snippets) {
      candidates.push({
        id: `snippet-${s.command}`,
        group: 'snippets',
        title: s.title,
        detail: s.command,
        action: () => onRunCommand?.(s.command),
      });
    }
    // Newest first, deduped, snippets excluded (they're listed above).
    const pinned = new Set(snippets.map((s) => s.command));
    for (const cmd of [...new Set([...recentCommands].reverse())]) {
      if (pinned.has(cmd)) continue;
      candidates.push({
        id: `recent-${cmd}`,
        group: 'recent',
        title: cmd,
        action: () => onRunCommand?.(cmd),
      });
    }
    if (onOpenAssist) {
      candidates.push(
        {
          id: 'assist-insights',
          group: 'actions',
          title: 'Open assist insights',
          action: () => onOpenAssist('insights'),
        },
        {
          id: 'assist-chat',
          group: 'actions',
          title: 'Open assist chat',
          action: () => onOpenAssist('chat'),
        },
      );
    }
    // The raw-run row always survives filtering (it *is* the query); the
    // rest rank by fuzzy match over title + detail.
    return candidates
      .map((item) => ({
        item,
        rank:
          item.id === 'raw-run'
            ? -1
            : fuzzyRank(query, `${item.title} ${item.detail ?? ''}`),
      }))
      .filter((r): r is { item: PaletteItem; rank: number } => r.rank !== null)
      .sort((a, b) => a.rank - b.rank)
      .map((r) => r.item);
  }, [open, query, snippets, recentCommands, onRunCommand, onOpenAssist]);

  useEffect(() => {
    if (cursor >= items.length) setCursor(Math.max(0, items.length - 1));
  }, [items.length, cursor]);

  // Keep the highlighted row scrolled into view.
  useEffect(() => {
    listRef.current
      ?.querySelector('[data-active="true"]')
      ?.scrollIntoView({ block: 'nearest' });
  }, [cursor, items]);

  if (!open) return null;

  const pick = (item: PaletteItem) => {
    item.action();
    onClose();
  };

  return (
    <div
      className="fixed inset-0 z-30 bg-black/50"
      onClick={onClose}
      data-testid="palette-backdrop"
    >
      <div
        data-testid="command-palette"
        role="dialog"
        aria-label="Command palette"
        onClick={(e) => e.stopPropagation()}
        className="mx-auto mt-[12vh] flex w-[34rem] max-w-[calc(100vw-2rem)] animate-nebula-fade-in flex-col overflow-hidden rounded-nebula-lg border border-white/10 bg-nebula-bg shadow-nebula-soft"
      >
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.preventDefault();
              onClose();
            } else if (e.key === 'ArrowDown') {
              e.preventDefault();
              setCursor((c) => Math.min(c + 1, items.length - 1));
            } else if (e.key === 'ArrowUp') {
              e.preventDefault();
              setCursor((c) => Math.max(c - 1, 0));
            } else if (e.key === 'Enter' && items[cursor]) {
              e.preventDefault();
              pick(items[cursor]);
            }
          }}
          placeholder="Type a command, snippet, or action…"
          aria-label="Palette query"
          data-testid="palette-input"
          className="border-b border-white/10 bg-nebula-surface px-4 py-3 font-nebula-command text-sm text-nebula-text placeholder:text-nebula-text/30 focus:outline-none"
        />
        <div ref={listRef} className="max-h-[40vh] overflow-y-auto p-1.5">
          {items.length === 0 && (
            <p className="px-3 py-4 font-nebula-meta text-xs text-nebula-text/40">
              Nothing matches.
            </p>
          )}
          {items.map((item, i) => (
            <button
              key={item.id}
              type="button"
              data-testid="palette-item"
              data-group={item.group}
              data-active={i === cursor}
              onClick={() => pick(item)}
              onMouseEnter={() => setCursor(i)}
              className={`flex w-full items-baseline gap-2 rounded-nebula-sm px-3 py-2 text-left transition-colors duration-nebula-fast ease-nebula ${
                i === cursor ? 'bg-nebula-accent/15' : 'hover:bg-white/5'
              }`}
            >
              {(i === 0 || items[i - 1].group !== item.group) && (
                <span className="w-24 shrink-0 font-nebula-meta text-[10px] uppercase tracking-wide text-nebula-text/30">
                  {GROUP_LABEL[item.group]}
                </span>
              )}
              {i > 0 && items[i - 1].group === item.group && <span className="w-24 shrink-0" />}
              <span className="truncate font-nebula-command text-sm text-nebula-text">
                {item.title}
              </span>
              {item.detail && (
                <span className="ml-auto truncate font-nebula-meta text-xs text-nebula-text/40">
                  {item.detail}
                </span>
              )}
            </button>
          ))}
        </div>
        <footer className="flex gap-3 border-t border-white/5 bg-nebula-surface px-4 py-1.5 font-nebula-meta text-[10px] text-nebula-text/30">
          <span>↑↓ navigate</span>
          <span>↵ run</span>
          <span>esc close</span>
        </footer>
      </div>
    </div>
  );
}

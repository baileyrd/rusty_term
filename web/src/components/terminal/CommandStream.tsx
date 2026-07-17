import { useEffect, useMemo, useRef, useState } from 'react';
import type { FormEvent } from 'react';
import CommandCard from './CommandCard';
import { groupCards, groupLabel } from './cardGroups';
import TerminalView from './TerminalView';
import type { CommandCardProps, SessionHandlers, SessionTabInfo } from './types';
import type { ThemeName } from '../../theme/tokens';

export interface CommandStreamProps {
  /** The active tab's command cards. */
  commands: CommandCardProps[];
  onCommandSubmit?: (command: string) => void;
  /** Pin a card's command to the side dock's snippets. */
  onPinCommand?: (command: string) => void;
  /** Per-tab live-session wiring for each tab's primary pane. */
  sessionHandlers?: SessionHandlers;
  /** Active preset, threaded to the xterm panels' runtime theme. */
  theme?: ThemeName;
  /**
   * Session tabs with their split-pane ids (primary first). Every pane is
   * its own terminal with its own transport session; only a tab's primary
   * pane feeds the command cards and the input line. Inactive tabs stay
   * mounted (hidden) so their sessions survive switches.
   */
  tabs: (SessionTabInfo & { panes: string[] })[];
  activeTabId: string;
  onTabSelect?: (id: string) => void;
  onTabAdd?: () => void;
  /** Close a tab (absent when only one remains). */
  onTabClose?: (id: string) => void;
  /** Close a secondary pane (the primary has no close affordance). */
  onClosePane?: (tabId: string, paneId: string) => void;
  /** Card to scroll into view and flash (a history-search jump target). */
  highlightCardId?: string | null;
}

/**
 * The center column: a scrolling stream of CommandCards, the raw xterm.js
 * panel, and the command input line at the bottom.
 */
export default function CommandStream({
  commands,
  onCommandSubmit,
  onPinCommand,
  sessionHandlers,
  theme,
  tabs,
  activeTabId,
  onTabSelect,
  onTabAdd,
  onTabClose,
  onClosePane,
  highlightCardId,
}: CommandStreamProps) {
  const [draft, setDraft] = useState('');
  const scrollRef = useRef<HTMLDivElement>(null);

  // Card groups: bursts of activity separated by idle gaps. Collapsed
  // group ids live here (memory only — a reload reopens everything);
  // headers only render once there's more than one group to fold.
  const groups = useMemo(() => groupCards(commands), [commands]);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const toggleGroup = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  // A search jump must be able to land inside a collapsed group.
  useEffect(() => {
    if (!highlightCardId) return;
    const target = groups.find((g) => g.cards.some((c) => c.id === highlightCardId));
    if (target && collapsed.has(target.id)) {
      setCollapsed((prev) => {
        const next = new Set(prev);
        next.delete(target.id);
        return next;
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- expand once per jump
  }, [highlightCardId, groups]);

  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [commands.length]);

  // Bring a search-jump target into view once it's rendered (collapsed is
  // a dependency so the scroll re-runs after a group auto-expands).
  useEffect(() => {
    if (!highlightCardId) return;
    scrollRef.current
      ?.querySelector(`[data-card-id="${CSS.escape(highlightCardId)}"]`)
      ?.scrollIntoView({ block: 'center' });
  }, [highlightCardId, collapsed]);

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    const cmd = draft.trim();
    if (cmd.length === 0) return;
    onCommandSubmit?.(cmd);
    setDraft('');
  }

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex shrink-0 items-center gap-1 border-b border-white/5 px-2 pt-1.5">
        {tabs.map((tab) => (
          <div
            key={tab.id}
            data-testid="session-tab"
            data-active={tab.id === activeTabId}
            className={`flex items-center gap-1.5 rounded-t-nebula-sm border border-b-0 px-3 py-1.5 font-nebula-meta text-xs transition-colors duration-nebula-fast ease-nebula ${
              tab.id === activeTabId
                ? 'border-white/10 bg-nebula-surface text-nebula-text'
                : 'border-transparent text-nebula-text/40 hover:text-nebula-text/70'
            }`}
          >
            <button type="button" onClick={() => onTabSelect?.(tab.id)} className="truncate">
              {tab.title}
            </button>
            {onTabClose && (
              <button
                type="button"
                data-testid="tab-close"
                aria-label={`Close ${tab.title}`}
                onClick={() => onTabClose(tab.id)}
                className="rounded-nebula-sm px-1 text-nebula-text/30 transition-colors duration-nebula-fast ease-nebula hover:bg-white/10 hover:text-nebula-error"
              >
                ✕
              </button>
            )}
          </div>
        ))}
        {onTabAdd && (
          <button
            type="button"
            data-testid="tab-add"
            aria-label="New session tab"
            onClick={onTabAdd}
            className="rounded-nebula-sm px-2 py-1 font-nebula-meta text-sm text-nebula-text/40 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5 hover:text-nebula-accent"
          >
            +
          </button>
        )}
      </div>
      <div ref={scrollRef} className="flex-1 space-y-3 overflow-y-auto p-4">
        {groups.map((group) => (
          <section key={group.id} className="space-y-3">
            {groups.length > 1 && (
              <button
                type="button"
                data-testid="card-group-header"
                data-collapsed={collapsed.has(group.id)}
                aria-expanded={!collapsed.has(group.id)}
                onClick={() => toggleGroup(group.id)}
                className="flex w-full items-center gap-2 rounded-nebula-sm px-1 py-0.5 text-left font-nebula-meta text-[11px] text-nebula-text/40 transition-colors duration-nebula-fast ease-nebula hover:text-nebula-text/70"
              >
                <span className="font-nebula-command text-nebula-accent/60">
                  {collapsed.has(group.id) ? '▸' : '▾'}
                </span>
                <span>{groupLabel(group)}</span>
                <span className="h-px flex-1 bg-white/5" />
              </button>
            )}
            {!collapsed.has(group.id) &&
              group.cards.map((c, i) => (
                <CommandCard
                  key={c.id ?? `cmd-${i}`}
                  {...c}
                  onPin={onPinCommand ? () => onPinCommand(c.command) : undefined}
                  onRerun={onCommandSubmit ? () => onCommandSubmit(c.command) : undefined}
                  highlighted={highlightCardId !== null && highlightCardId === c.id}
                />
              ))}
          </section>
        ))}

        {tabs.map((tab) => (
          // Every tab's panes stay mounted so switching never drops a
          // session; inactive tabs are just display:none.
          <div key={tab.id} className={tab.id === activeTabId ? 'flex gap-3' : 'hidden'}>
            {tab.panes.map((id, i) => (
              <TerminalView
                key={id}
                theme={theme}
                // Only the primary pane feeds the card stream and input
                // line; splits are independent sessions on the same bridge.
                onCommandEvent={i === 0 ? sessionHandlers?.(tab.id).onCommandEvent : undefined}
                onTransportReady={i === 0 ? sessionHandlers?.(tab.id).onTransportReady : undefined}
                title={tab.panes.length > 1 ? `pane ${i + 1}` : 'raw terminal'}
                onClose={i > 0 && onClosePane ? () => onClosePane(tab.id, id) : undefined}
              />
            ))}
          </div>
        ))}
      </div>

      <form
        onSubmit={handleSubmit}
        className="flex shrink-0 items-center gap-2 border-t border-white/5 bg-nebula-surface px-4 py-3"
      >
        <span className="select-none font-nebula-command text-sm text-nebula-accent2">❯</span>
        <input
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder="Type a command…"
          spellCheck={false}
          autoComplete="off"
          className="flex-1 bg-transparent font-nebula-command text-sm text-nebula-text caret-nebula-accent outline-none transition-colors duration-nebula-fast ease-nebula placeholder:text-nebula-text/25"
          aria-label="Command input"
        />
      </form>
    </main>
  );
}

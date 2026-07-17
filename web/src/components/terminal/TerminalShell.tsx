import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import StatusRibbon from './StatusRibbon';
import CommandStream from './CommandStream';
import SideDock from './SideDock';
import AiOrb from './AiOrb';
import AssistPanel, { type AiAssistState, type ChatState } from './AssistPanel';
import CommandPalette from './CommandPalette';
import SearchOverlay from './SearchOverlay';
import { exportTranscript } from './transcript';
import { localHeuristics } from '../../assist/heuristics';
import {
  createLlmProvider,
  loadApiKey,
  storeApiKey,
  type ChatMessage,
} from '../../assist/llmProvider';
import { THEME_NAMES, type ThemeName } from '../../theme/tokens';
import { applyTheme } from '../../theme/apply';
import type { SnippetItem, TerminalShellProps } from './types';

const DEFAULT_TABS = [{ id: 'main', title: 'session 1' }];

/** localStorage key for the pinned snippets. */
const SNIPPETS_KEY = 'nebula.pinnedSnippets';

/** localStorage key for the chosen theme preset. */
const THEME_KEY = 'nebula.theme';

/** Ceiling for side-by-side terminal panes — beyond this they get too thin. */
const MAX_PANES = 4;

/** localStorage key for the per-tab pane layouts (tabs/cards live with the app). */
const PANES_KEY = 'nebula.panes';

function loadPanes(): Record<string, string[]> {
  try {
    const raw = localStorage.getItem(PANES_KEY);
    if (raw === null) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (
      typeof parsed === 'object' &&
      parsed !== null &&
      Object.values(parsed).every(
        (v) => Array.isArray(v) && v.every((p) => typeof p === 'string'),
      )
    ) {
      return parsed as Record<string, string[]>;
    }
  } catch {
    // Corrupt or unavailable storage: start with single-pane layouts.
  }
  return {};
}

/** The next free `pane-N` counter given the restored layouts. */
function nextPaneCounter(panesByTab: Record<string, string[]>): number {
  const ids = Object.values(panesByTab).flat();
  const max = Math.max(0, ...ids.map((id) => Number(/^pane-(\d+)$/.exec(id)?.[1] ?? 0)));
  return max + 1;
}

function loadTheme(): ThemeName | null {
  try {
    const stored = localStorage.getItem(THEME_KEY);
    return THEME_NAMES.includes(stored as ThemeName) ? (stored as ThemeName) : null;
  } catch {
    return null;
  }
}

const DEFAULT_SNIPPETS: SnippetItem[] = [
  { title: 'Rebuild + test', command: 'cargo test --workspace' },
  { title: 'Tail logs', command: 'journalctl -fu rusty-term-bridge' },
];

function loadSnippets(): SnippetItem[] {
  try {
    const raw = localStorage.getItem(SNIPPETS_KEY);
    if (raw === null) return DEFAULT_SNIPPETS;
    const parsed = JSON.parse(raw) as unknown;
    if (
      Array.isArray(parsed) &&
      parsed.every(
        (s) =>
          typeof s === 'object' &&
          s !== null &&
          typeof (s as SnippetItem).title === 'string' &&
          typeof (s as SnippetItem).command === 'string',
      )
    ) {
      return parsed as SnippetItem[];
    }
  } catch {
    // Corrupt or unavailable storage: fall through to defaults.
  }
  return DEFAULT_SNIPPETS;
}

/** A dock title for a pinned command: its first couple of words. */
function snippetTitle(command: string): string {
  const words = command.trim().split(/\s+/);
  return words.slice(0, 2).join(' ');
}

/**
 * Layout root for the Nebula terminal: status ribbon on top, command stream
 * in the center, side dock on the right, AI orb floating bottom-right.
 *
 * Owns the two pieces of cross-cutting UI state: the pinned snippets
 * (persisted to localStorage; pin from a card's hover button, run/unpin in
 * the dock) and the assist panel (the orb's sheet — local heuristics always,
 * plus a Claude-backed section when an API key is connected; its badge
 * counts failures that arrived since the panel was last opened).
 *
 * The `theme` prop is the initial preset only; the palette's "Theme: …"
 * actions switch between nebula / cyberpunk / minimal at runtime and the
 * choice persists in localStorage (which wins over the prop on reload).
 */
export default function TerminalShell({
  theme = 'nebula',
  commands = [],
  onCommandSubmit,
  sessionHandlers,
  tabs = DEFAULT_TABS,
  activeTabId,
  onTabSelect,
  onTabAdd,
  onTabClose,
  liveStats,
  searchSessions,
  tabBadges,
}: TerminalShellProps) {
  const currentTab = activeTabId ?? tabs[0].id;
  // The active preset: stored choice wins over the prop, which stays the
  // initial default. applyTheme stamps the custom properties on <html> so
  // Tailwind's var()-based colors (and the body background) all follow.
  const [activeTheme, setActiveTheme] = useState<ThemeName>(() => loadTheme() ?? theme);
  useEffect(() => {
    applyTheme(activeTheme);
    try {
      localStorage.setItem(THEME_KEY, activeTheme);
    } catch {
      // Blocked storage: the choice just doesn't survive a reload.
    }
  }, [activeTheme]);

  // Split panes, per tab: primary first, up to MAX_PANES side-by-side
  // terminals. Each split is its own transport session; ids only ever grow
  // so React never remounts (and thus never reconnects) a surviving pane.
  const [panesByTab, setPanesByTab] = useState<Record<string, string[]>>(loadPanes);
  const paneCounter = useRef(nextPaneCounter(panesByTab));

  // Persist the layouts, pruned to tabs that still exist (stale keys can
  // linger from workspaces closed in other circumstances).
  useEffect(() => {
    try {
      localStorage.setItem(
        PANES_KEY,
        JSON.stringify(
          Object.fromEntries(
            tabs.filter((t) => panesByTab[t.id]).map((t) => [t.id, panesByTab[t.id]]),
          ),
        ),
      );
    } catch {
      // Blocked storage: the layout just doesn't survive a reload.
    }
  }, [panesByTab, tabs]);
  const panesFor = useCallback(
    (tabId: string) => panesByTab[tabId] ?? ['primary'],
    [panesByTab],
  );
  const splitPane = useCallback(() => {
    setPanesByTab((prev) => {
      const cur = prev[currentTab] ?? ['primary'];
      return cur.length >= MAX_PANES
        ? prev
        : { ...prev, [currentTab]: [...cur, `pane-${paneCounter.current++}`] };
    });
  }, [currentTab]);
  const closePane = useCallback((tabId: string, id: string) => {
    setPanesByTab((prev) => {
      const cur = prev[tabId] ?? ['primary'];
      if (cur.length <= 1 || id === 'primary') return prev;
      return { ...prev, [tabId]: cur.filter((p) => p !== id) };
    });
  }, []);
  const closeLastPane = useCallback(() => {
    setPanesByTab((prev) => {
      const cur = prev[currentTab] ?? ['primary'];
      return cur.length > 1 ? { ...prev, [currentTab]: cur.slice(0, -1) } : prev;
    });
  }, [currentTab]);
  const handleTabClose = useCallback(
    (id: string) => {
      // Drop the closed tab's pane layout with it.
      setPanesByTab((prev) => {
        const { [id]: _dropped, ...rest } = prev;
        return rest;
      });
      onTabClose?.(id);
    },
    [onTabClose],
  );

  // Failures-only view of the card stream (per-sitting, palette-toggled).
  const [failuresOnly, setFailuresOnly] = useState(false);

  const [snippets, setSnippets] = useState<SnippetItem[]>(loadSnippets);
  const [assistOpen, setAssistOpen] = useState(false);
  const [seenFailures, setSeenFailures] = useState(0);
  const [apiKey, setApiKey] = useState<string | null>(loadApiKey);
  const [aiState, setAiState] = useState<AiAssistState>({ phase: 'disconnected' });
  // Monotonic id so a slow response from a stale request can't clobber state.
  const aiRequest = useRef(0);

  useEffect(() => {
    try {
      localStorage.setItem(SNIPPETS_KEY, JSON.stringify(snippets));
    } catch {
      // Storage full/blocked: pins simply don't persist this session.
    }
  }, [snippets]);

  const insights = useMemo(() => localHeuristics.analyze(commands), [commands]);

  // The Claude half: with a key connected and the panel open, ship the
  // current cards off for analysis. Re-runs when a command finishes (the
  // dependency is the finished count, not the array identity, so streaming
  // output doesn't hammer the API), guarded against stale responses.
  const finishedCount = useMemo(
    () => commands.filter((c) => c.status === 'success' || c.status === 'error').length,
    [commands],
  );
  useEffect(() => {
    if (apiKey === null) {
      setAiState({ phase: 'disconnected' });
      return;
    }
    if (!assistOpen) return;
    const requestId = ++aiRequest.current;
    setAiState({ phase: 'loading' });
    createLlmProvider(apiKey)
      .analyze(commands, (partial) => {
        if (aiRequest.current === requestId) setAiState({ phase: 'streaming', insights: partial });
      })
      .then((aiInsights) => {
        if (aiRequest.current === requestId) setAiState({ phase: 'ready', insights: aiInsights });
      })
      .catch((err: unknown) => {
        if (aiRequest.current === requestId) {
          setAiState({ phase: 'error', message: err instanceof Error ? err.message : String(err) });
        }
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps -- see note above on finishedCount
  }, [apiKey, assistOpen, finishedCount]);

  const connectAssist = useCallback((key: string) => {
    storeApiKey(key);
    setApiKey(key);
  }, []);

  const disconnectAssist = useCallback(() => {
    storeApiKey(null);
    setApiKey(null);
    setChat({ messages: [], busy: false });
  }, []);

  // The chat thread. The last assistant message is appended empty when a
  // send starts and grows with each streamed delta; refs give the async
  // handler the current thread and cards without re-creating the callback.
  const [chat, setChat] = useState<ChatState>({ messages: [], busy: false });
  const chatRef = useRef(chat);
  chatRef.current = chat;
  const commandsRef = useRef(commands);
  commandsRef.current = commands;
  const apiKeyRef = useRef(apiKey);
  apiKeyRef.current = apiKey;

  const sendChat = useCallback((text: string) => {
    const key = apiKeyRef.current;
    if (key === null || chatRef.current.busy) return;
    const history: ChatMessage[] = [...chatRef.current.messages, { role: 'user', text }];
    setChat({ messages: [...history, { role: 'assistant', text: '' }], busy: true });
    const patchReply = (reply: string) =>
      setChat((prev) => ({
        ...prev,
        messages: [...prev.messages.slice(0, -1), { role: 'assistant', text: reply }],
      }));
    createLlmProvider(key)
      .chat(history, commandsRef.current, patchReply)
      .then((reply) => {
        patchReply(reply);
        setChat((prev) => ({ ...prev, busy: false, error: undefined }));
      })
      .catch((err: unknown) => {
        setChat({
          // Drop the empty assistant stub; keep the user's turn for a retry.
          messages: history,
          busy: false,
          error: err instanceof Error ? err.message : String(err),
        });
      });
  }, []);
  const failures = useMemo(
    () => commands.filter((c) => c.status === 'error').length,
    [commands],
  );
  const unreadHints = assistOpen ? 0 : Math.max(0, failures - seenFailures);

  const pinCommand = useCallback((command: string) => {
    setSnippets((prev) =>
      prev.some((s) => s.command === command)
        ? prev
        : [...prev, { title: snippetTitle(command), command }],
    );
  }, []);

  const removeSnippet = useCallback((snippet: SnippetItem) => {
    setSnippets((prev) => prev.filter((s) => s.command !== snippet.command));
  }, []);

  const toggleAssist = useCallback(() => {
    setAssistOpen((open) => {
      if (!open) setSeenFailures(failures);
      return !open;
    });
  }, [failures]);

  // The Ctrl/Cmd+K command palette and Ctrl/Cmd+Shift+F history search.
  // Listeners run in the capture phase and swallow the chords so a focused
  // xterm never writes them into the pty.
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
  const [assistTab, setAssistTab] = useState<'insights' | 'chat'>('insights');
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key.toLowerCase() === 'k' && (e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey) {
        e.preventDefault();
        e.stopPropagation();
        setPaletteOpen((open) => !open);
      } else if (e.key.toLowerCase() === 'f' && (e.ctrlKey || e.metaKey) && e.shiftKey && !e.altKey) {
        e.preventDefault();
        e.stopPropagation();
        setSearchOpen((open) => !open);
      }
    };
    window.addEventListener('keydown', onKeyDown, true);
    return () => window.removeEventListener('keydown', onKeyDown, true);
  }, []);

  // A search hit was jumped to: its card gets scrolled into view and
  // flashed by the stream; the flash clears itself.
  const [highlightCardId, setHighlightCardId] = useState<string | null>(null);
  useEffect(() => {
    if (highlightCardId === null) return;
    const timer = setTimeout(() => setHighlightCardId(null), 2000);
    return () => clearTimeout(timer);
  }, [highlightCardId]);
  const jumpToCard = useCallback(
    (tabId: string, cardId: string) => {
      if (tabId !== currentTab) onTabSelect?.(tabId);
      // The target may not be a failure — drop the filter so it can render.
      setFailuresOnly(false);
      setHighlightCardId(cardId);
    },
    [currentTab, onTabSelect],
  );

  const openAssistFromPalette = useCallback(
    (tab: 'insights' | 'chat') => {
      setAssistTab(tab);
      setSeenFailures(failures);
      setAssistOpen(true);
    },
    [failures],
  );

  // Demo ribbon/dock data, used when no live stats channel is feeding us.
  const demoLoad = [0.22, 0.31, 0.28, 0.45, 0.38, 0.52, 0.47, 0.6, 0.42, 0.35, 0.4, 0.33];
  const live = liveStats;

  return (
    <div
      data-theme={activeTheme}
      className="flex h-full flex-col bg-nebula-bg text-nebula-text"
    >
      <StatusRibbon
        systemLoad={live && live.systemLoad.length > 1 ? live.systemLoad : demoLoad}
        latencyMs={live ? (live.latencyMs ?? 0) : 12}
        environment={live ? 'live' : 'demo'}
        gitBranch={live ? (live.gitBranch ?? '(no repo)') : 'claude/rusty-term-web-frontend'}
        gitStats={live ? live.gitStats : { added: 3, modified: 7, deleted: 1 }}
      />

      <div className="flex min-h-0 flex-1">
        <CommandStream
          commands={commands}
          onCommandSubmit={onCommandSubmit}
          onPinCommand={pinCommand}
          sessionHandlers={sessionHandlers}
          theme={activeTheme}
          tabs={tabs.map((t) => ({ ...t, panes: panesFor(t.id) }))}
          activeTabId={currentTab}
          onTabSelect={onTabSelect}
          onTabAdd={onTabAdd}
          onTabClose={tabs.length > 1 ? handleTabClose : undefined}
          onClosePane={closePane}
          tabBadges={tabBadges}
          highlightCardId={highlightCardId}
          failuresOnly={failuresOnly}
          onClearFilter={() => setFailuresOnly(false)}
        />
        <SideDock
          cpu={live ? (live.cpu ?? 0) : 0.34}
          ram={live ? (live.ram ?? 0) : 0.61}
          recentCommands={commands.map((c) => c.command).slice(-6).reverse()}
          pinnedSnippets={snippets}
          onSnippetClick={(s) => onCommandSubmit?.(s.command)}
          onSnippetRemove={removeSnippet}
          onRecentCommandClick={(cmd) => onCommandSubmit?.(cmd)}
        />
      </div>

      <SearchOverlay
        open={searchOpen}
        onClose={() => setSearchOpen(false)}
        sessions={
          searchSessions ?? [
            { id: currentTab, title: tabs.find((t) => t.id === currentTab)?.title ?? 'session', commands },
          ]
        }
        onJump={jumpToCard}
      />

      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        snippets={snippets}
        recentCommands={commands.map((c) => c.command)}
        onRunCommand={onCommandSubmit}
        onOpenAssist={openAssistFromPalette}
        activeTheme={activeTheme}
        onSetTheme={setActiveTheme}
        paneCount={panesFor(currentTab).length}
        onSplitPane={splitPane}
        onCloseLastPane={closeLastPane}
        tabs={tabs}
        activeTabId={currentTab}
        onTabSelect={onTabSelect}
        onTabAdd={onTabAdd}
        onTabClose={tabs.length > 1 ? () => handleTabClose(currentTab) : undefined}
        onSearchHistory={() => setSearchOpen(true)}
        failuresOnly={failuresOnly}
        onToggleFailuresOnly={() => setFailuresOnly((v) => !v)}
        onExportTranscript={(format) =>
          exportTranscript(
            tabs.find((t) => t.id === currentTab)?.title ?? 'session',
            commands,
            format,
          )
        }
      />

      {assistOpen && (
        <AssistPanel
          insights={insights}
          ai={aiState}
          initialTab={assistTab}
          chat={chat}
          onChatSend={sendChat}
          onChatRun={onCommandSubmit}
          onConnect={connectAssist}
          onDisconnect={disconnectAssist}
          onRun={
            onCommandSubmit
              ? (cmd) => {
                  onCommandSubmit(cmd);
                  setAssistOpen(false);
                }
              : undefined
          }
          onClose={toggleAssist}
        />
      )}
      <AiOrb unreadHints={unreadHints} enabled onClick={toggleAssist} />
    </div>
  );
}

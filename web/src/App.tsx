import { useCallback, useEffect, useRef, useState } from 'react';
import type { CommandEvent } from './components/terminal/commandTracker';
import TerminalShell from './components/terminal/TerminalShell';
import type { CommandCardProps, LiveShellStats } from './components/terminal/types';
import type { TerminalTransport } from './transport/bridge';

/**
 * Live mode: the page was opened with `?ws[=url]`, so the raw terminal is a
 * real shell behind `rusty_term_web_bridge`. The command cards are then fed
 * by the session's OSC 133 marks instead of demo data, and the input line
 * writes into the same PTY.
 */
const LIVE = new URLSearchParams(window.location.search).has('ws');

const now = Date.now();

const DEMO_COMMANDS: CommandCardProps[] = [
  {
    id: 'demo-1',
    command: "git commit -m 'Nebula theme preset + bottom status ribbon'",
    status: 'success',
    output: [
      '[main 5b08146] Nebula theme preset + bottom status ribbon',
      ' 9 files changed, 412 insertions(+), 38 deletions(-)',
    ],
    meta: 'exit 0',
    startedAt: now - 320_000,
    finishedAt: now - 319_780,
  },
  {
    id: 'demo-2',
    command: './deploy.sh --env staging',
    status: 'success',
    output: [
      '→ building rusty_term v0.9.3 (release)',
      '→ uploading artifact (14.2 MB)',
      '→ rolling staging fleet… 4/4 healthy',
      '✔ deployed in 42.7s',
    ],
    meta: 'exit 0 · staging',
    startedAt: now - 180_000,
    finishedAt: now - 137_300,
  },
  {
    id: 'demo-3',
    command: 'rm /var/log/rusty_term/session.lock',
    status: 'error',
    output: [
      "rm: cannot remove '/var/log/rusty_term/session.lock': Permission denied",
    ],
    meta: 'exit 1',
    startedAt: now - 60_000,
    finishedAt: now - 59_960,
  },
];

/** Cap on retained cards; the terminal's scrollback keeps the rest. */
const CARDS_MAX = 100;

/** Load samples retained for the ribbon's sparkline. */
const LOAD_HISTORY = 12;

/** One OSC 133 event folded into a tab's card list (pure). */
function reduceCommandEvent(prev: CommandCardProps[], event: CommandEvent): CommandCardProps[] {
  if (event.type === 'start') {
    const card: CommandCardProps = {
      id: `live-${event.startedAt}-${prev.length}`,
      command: event.command,
      status: 'running',
      output: [],
      startedAt: event.startedAt,
    };
    return [...prev, card].slice(-CARDS_MAX);
  }
  // finish: close the most recent running card.
  const i = prev.map((c) => c.status).lastIndexOf('running');
  if (i < 0) return prev;
  const open = prev[i];
  const secs = open.startedAt ? (event.finishedAt - open.startedAt) / 1000 : null;
  const done: CommandCardProps = {
    ...open,
    // No reported code reads as "finished" rather than failed, the same
    // call the native gutter marks make.
    status: event.exit === null || event.exit === 0 ? 'success' : 'error',
    output: event.output,
    finishedAt: event.finishedAt,
    meta: [
      event.exit === null ? 'exit ?' : `exit ${event.exit}`,
      secs !== null ? `${secs.toFixed(secs < 10 ? 1 : 0)}s` : null,
    ]
      .filter(Boolean)
      .join(' · '),
  };
  return [...prev.slice(0, i), done, ...prev.slice(i + 1)];
}

interface SessionTab {
  id: string;
  title: string;
}

/** localStorage key for the tab/card workspace (pane layouts live with the shell). */
const SESSION_KEY = 'nebula.session';

/** Lines of output retained per card in the saved session. */
const SAVED_OUTPUT_LINES = 30;

interface SavedSession {
  tabs: SessionTab[];
  activeTabId: string;
  commandsByTab: Record<string, CommandCardProps[]>;
}

/**
 * A card as it comes back from storage: a command that was still running
 * when the page went away can't be resumed — it reads as interrupted.
 */
function reviveCard(card: CommandCardProps): CommandCardProps {
  if (card.status !== 'running') return card;
  return { ...card, status: 'idle', meta: 'interrupted' };
}

function loadSession(): SavedSession | null {
  try {
    const raw = localStorage.getItem(SESSION_KEY);
    if (raw === null) return null;
    const parsed = JSON.parse(raw) as SavedSession;
    if (
      !Array.isArray(parsed.tabs) ||
      parsed.tabs.length === 0 ||
      !parsed.tabs.every((t) => typeof t?.id === 'string' && typeof t?.title === 'string') ||
      typeof parsed.activeTabId !== 'string' ||
      typeof parsed.commandsByTab !== 'object' ||
      parsed.commandsByTab === null
    ) {
      return null;
    }
    return {
      tabs: parsed.tabs,
      activeTabId: parsed.tabs.some((t) => t.id === parsed.activeTabId)
        ? parsed.activeTabId
        : parsed.tabs[0].id,
      commandsByTab: Object.fromEntries(
        parsed.tabs.map((t) => [
          t.id,
          (Array.isArray(parsed.commandsByTab[t.id]) ? parsed.commandsByTab[t.id] : []).map(
            reviveCard,
          ),
        ]),
      ),
    };
  } catch {
    return null;
  }
}

/** The next free `tab-N` counter given the restored tabs. */
function nextTabCounter(tabs: SessionTab[]): number {
  const max = Math.max(0, ...tabs.map((t) => Number(/^tab-(\d+)$/.exec(t.id)?.[1] ?? 0)));
  return max + 1;
}

export default function App() {
  // Session tabs: each is an independent workspace — its own command cards,
  // its own transport (and thus its own PTY session in live mode), its own
  // pane set (owned by the shell). Tab 1 gets the demo cards; new tabs
  // start clean.
  // Restored workspace, when one was saved. The card history and layout
  // come back; live PTY sessions cannot — each tab reconnects fresh.
  const [saved] = useState(loadSession);
  const [tabs, setTabs] = useState<SessionTab[]>(
    saved?.tabs ?? [{ id: 'tab-1', title: 'session 1' }],
  );
  const [activeTabId, setActiveTabId] = useState(saved?.activeTabId ?? 'tab-1');
  const [commandsByTab, setCommandsByTab] = useState<Record<string, CommandCardProps[]>>(
    saved?.commandsByTab ?? { 'tab-1': LIVE ? [] : DEMO_COMMANDS },
  );
  const [liveStats, setLiveStats] = useState<LiveShellStats | undefined>(undefined);
  const transportsRef = useRef(new Map<string, TerminalTransport>());
  const tabCounter = useRef(saved ? nextTabCounter(saved.tabs) : 2);

  // Debounced save: card streams update on every OSC 133 event, so batch
  // writes; output is trimmed per card to bound what storage holds.
  useEffect(() => {
    const timer = setTimeout(() => {
      try {
        localStorage.setItem(
          SESSION_KEY,
          JSON.stringify({
            tabs,
            activeTabId,
            commandsByTab: Object.fromEntries(
              tabs.map((t) => [
                t.id,
                (commandsByTab[t.id] ?? []).map((c) => ({
                  ...c,
                  output: c.output.slice(-SAVED_OUTPUT_LINES),
                })),
              ]),
            ),
          } satisfies SavedSession),
        );
      } catch {
        // Storage full/blocked: the workspace just won't survive a reload.
      }
    }, 400);
    return () => clearTimeout(timer);
  }, [tabs, activeTabId, commandsByTab]);

  /** OSC 133 events from a tab's live session → that tab's command cards. */
  const handleCommandEvent = useCallback((tabId: string, event: CommandEvent) => {
    setCommandsByTab((prev) => ({
      ...prev,
      [tabId]: reduceCommandEvent(prev[tabId] ?? [], event),
    }));
  }, []);

  const handleTransportReady = useCallback((tabId: string, t: TerminalTransport) => {
    transportsRef.current.set(tabId, t);
    // Bridge stats pushes → ribbon/dock state, with a rolling load history
    // for the sparkline. All tabs talk to the same host, so last write
    // wins harmlessly. The subscription dies with the transport.
    t.onStats?.((s) => {
      setLiveStats((prev) => ({
        systemLoad:
          s.load === null
            ? (prev?.systemLoad ?? [])
            : [...(prev?.systemLoad ?? []), Math.min(s.load, 1)].slice(-LOAD_HISTORY),
        latencyMs: s.latencyMs,
        gitBranch: s.branch,
        gitStats: s.git,
        cpu: s.load === null ? null : Math.min(s.load, 1),
        ram: s.mem,
      }));
    });
  }, []);

  // Stable per-tab handler objects: TerminalView's mount effect depends on
  // these callbacks, so fresh identities would tear down and reconnect the
  // pane's session on every render.
  const handlersRef = useRef(
    new Map<
      string,
      {
        onCommandEvent: (event: CommandEvent) => void;
        onTransportReady: (t: TerminalTransport) => void;
      }
    >(),
  );
  const sessionHandlers = useCallback(
    (tabId: string) => {
      let h = handlersRef.current.get(tabId);
      if (!h) {
        h = {
          onCommandEvent: (event: CommandEvent) => handleCommandEvent(tabId, event),
          onTransportReady: (t: TerminalTransport) => handleTransportReady(tabId, t),
        };
        handlersRef.current.set(tabId, h);
      }
      return h;
    },
    [handleCommandEvent, handleTransportReady],
  );

  const addTab = useCallback(() => {
    const n = tabCounter.current++;
    const id = `tab-${n}`;
    setTabs((prev) => [...prev, { id, title: `session ${n}` }]);
    setCommandsByTab((prev) => ({ ...prev, [id]: [] }));
    setActiveTabId(id);
  }, []);

  const closeTab = useCallback(
    (id: string) => {
      if (tabs.length <= 1) return;
      const idx = tabs.findIndex((t) => t.id === id);
      if (idx < 0) return;
      const next = tabs.filter((t) => t.id !== id);
      setTabs(next);
      if (activeTabId === id) setActiveTabId(next[Math.max(0, idx - 1)].id);
      setCommandsByTab((prev) => {
        const { [id]: _dropped, ...rest } = prev;
        return rest;
      });
      // The tab's TerminalViews unmount and dispose their own transports;
      // just forget our references.
      transportsRef.current.delete(id);
      handlersRef.current.delete(id);
    },
    [tabs, activeTabId],
  );

  /**
   * The input line, targeting the active tab. Live: write the command into
   * that tab's PTY (the OSC 133 marks then produce its card, exactly as if
   * it were typed in the raw panel). Demo: append a fake card, as before.
   */
  const handleSubmit = useCallback(
    (command: string) => {
      if (LIVE) {
        transportsRef.current.get(activeTabId)?.write(`${command}\r`);
        return;
      }
      const startedAt = Date.now();
      setCommandsByTab((prev) => ({
        ...prev,
        [activeTabId]: [
          ...(prev[activeTabId] ?? []),
          {
            id: `local-${startedAt}`,
            command,
            status: 'success',
            output: [`(demo) executed locally: ${command}`],
            meta: 'exit 0 · loopback',
            startedAt,
            finishedAt: startedAt + 12,
          },
        ],
      }));
    },
    [activeTabId],
  );

  return (
    <TerminalShell
      theme="nebula"
      commands={commandsByTab[activeTabId] ?? []}
      onCommandSubmit={handleSubmit}
      sessionHandlers={LIVE ? sessionHandlers : undefined}
      tabs={tabs}
      activeTabId={activeTabId}
      onTabSelect={setActiveTabId}
      onTabAdd={addTab}
      onTabClose={closeTab}
      liveStats={liveStats}
      searchSessions={tabs.map((t) => ({
        id: t.id,
        title: t.title,
        commands: commandsByTab[t.id] ?? [],
      }))}
    />
  );
}

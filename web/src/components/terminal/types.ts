/** Shared prop types for the Nebula terminal components, per the design spec. */

export type CommandStatus = 'idle' | 'running' | 'success' | 'error';

export interface CommandCardProps {
  id?: string;
  command: string;
  status: CommandStatus;
  output: string[];
  meta?: string;
  startedAt?: number;
  finishedAt?: number;
}

export interface GitStats {
  added: number;
  modified: number;
  deleted: number;
}

export interface StatusRibbonProps {
  /** Recent system-load samples (0..1), newest last, for the sparkline. */
  systemLoad: number[];
  latencyMs: number;
  environment: string;
  gitBranch: string;
  gitStats: GitStats;
}

export interface SnippetItem {
  title: string;
  command: string;
}

export interface SideDockProps {
  /** CPU utilization 0..1. */
  cpu: number;
  /** RAM utilization 0..1. */
  ram: number;
  recentCommands?: string[];
  pinnedSnippets?: SnippetItem[];
  /** Run a snippet (the design spec's snippet-clicked event). */
  onSnippetClick?: (snippet: SnippetItem) => void;
  /** Unpin a snippet from the dock. */
  onSnippetRemove?: (snippet: SnippetItem) => void;
  /** Re-run a recent command. */
  onRecentCommandClick?: (command: string) => void;
}

export interface AiOrbProps {
  unreadHints?: number;
  enabled?: boolean;
  onClick?: () => void;
}

/**
 * Live session stats assembled by the app from the bridge's `stats` pushes
 * (see `transport/bridge.ts`): a rolling load history for the sparkline,
 * measured latency, and git facts for the shell's cwd. `null` fields mean
 * "the host couldn't provide this" and fall back to the demo values.
 */
export interface LiveShellStats {
  systemLoad: number[];
  latencyMs: number | null;
  gitBranch: string | null;
  gitStats: GitStats;
  cpu: number | null;
  ram: number | null;
}

export interface SessionTabInfo {
  id: string;
  title: string;
}

/**
 * Per-session wiring for a tab's primary terminal pane: structured OSC 133
 * command events and the connected transport, threaded down to
 * `TerminalView` / up to the app so each tab's command cards are fed by
 * its own shell session. Returned objects must be identity-stable per tab
 * — `TerminalView`'s mount effect depends on them.
 */
export type SessionHandlers = (tabId: string) => {
  onCommandEvent: (event: import('./commandTracker').CommandEvent) => void;
  onTransportReady: (transport: import('../../transport/bridge').TerminalTransport) => void;
};

export interface TerminalShellProps {
  theme?: 'nebula' | 'cyberpunk' | 'minimal';
  /** The active tab's command cards. */
  commands?: CommandCardProps[];
  onCommandSubmit?: (command: string) => void;
  /** Live-session wiring; absent in demo mode. */
  sessionHandlers?: SessionHandlers;
  /** Session tabs, primary first. Defaults to a single anonymous session. */
  tabs?: SessionTabInfo[];
  activeTabId?: string;
  onTabSelect?: (id: string) => void;
  onTabAdd?: () => void;
  onTabClose?: (id: string) => void;
  /** Live ribbon/dock stats; absent (demo mode) keeps the hardcoded values. */
  liveStats?: LiveShellStats;
  /**
   * Every tab's card history, for the Ctrl+Shift+F search overlay. Absent
   * means "search only what `commands` shows".
   */
  searchSessions?: { id: string; title: string; commands: CommandCardProps[] }[];
}

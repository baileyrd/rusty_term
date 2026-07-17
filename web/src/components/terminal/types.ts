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

export interface TerminalShellProps {
  theme?: 'nebula' | 'cyberpunk' | 'minimal';
  commands?: CommandCardProps[];
  onCommandSubmit?: (command: string) => void;
  /**
   * Live-session extensions (additive to the original spec): structured
   * OSC 133 command events from the raw terminal, and the connected
   * transport — both threaded down to `TerminalView` / up to the app so
   * the command cards can be fed by a real shell session.
   */
  onCommandEvent?: (event: import('./commandTracker').CommandEvent) => void;
  onTransportReady?: (transport: import('../../transport/bridge').TerminalTransport) => void;
  /** Live ribbon/dock stats; absent (demo mode) keeps the hardcoded values. */
  liveStats?: LiveShellStats;
}

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
}

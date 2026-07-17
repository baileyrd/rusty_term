import { useState } from 'react';
import StatusRibbon from './StatusRibbon';
import CommandStream from './CommandStream';
import SideDock from './SideDock';
import AiOrb from './AiOrb';
import type { TerminalShellProps } from './types';

/**
 * Layout root for the Nebula terminal: status ribbon on top, command stream
 * in the center, side dock on the right, AI orb floating bottom-right.
 *
 * The `theme` prop is Nebula-only for now; 'cyberpunk' and 'minimal' are
 * accepted per the spec but map to the Nebula skin until those presets land.
 */
export default function TerminalShell({
  theme = 'nebula',
  commands = [],
  onCommandSubmit,
  onCommandEvent,
  onTransportReady,
  liveStats,
}: TerminalShellProps) {
  const [orbHints, setOrbHints] = useState(2);

  // Demo ribbon/dock data, used when no live stats channel is feeding us.
  const demoLoad = [0.22, 0.31, 0.28, 0.45, 0.38, 0.52, 0.47, 0.6, 0.42, 0.35, 0.4, 0.33];
  const live = liveStats;

  return (
    <div
      data-theme={theme}
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
          onCommandEvent={onCommandEvent}
          onTransportReady={onTransportReady}
        />
        <SideDock
          cpu={live ? (live.cpu ?? 0) : 0.34}
          ram={live ? (live.ram ?? 0) : 0.61}
          recentCommands={commands.map((c) => c.command).slice(-6).reverse()}
          pinnedSnippets={[
            { title: 'Rebuild + test', command: 'cargo test --workspace' },
            { title: 'Tail logs', command: 'journalctl -fu rusty-term-bridge' },
          ]}
        />
      </div>

      <AiOrb unreadHints={orbHints} enabled onClick={() => setOrbHints(0)} />
    </div>
  );
}

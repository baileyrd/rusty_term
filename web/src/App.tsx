import { useCallback, useState } from 'react';
import TerminalShell from './components/terminal/TerminalShell';
import type { CommandCardProps } from './components/terminal/types';

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

export default function App() {
  const [commands, setCommands] = useState<CommandCardProps[]>(DEMO_COMMANDS);

  // Demo submit handler: append an echo card. The real build routes this
  // through the websocket PTY bridge instead.
  const handleSubmit = useCallback((command: string) => {
    const startedAt = Date.now();
    setCommands((prev) => [
      ...prev,
      {
        id: `local-${startedAt}`,
        command,
        status: 'success',
        output: [`(demo) executed locally: ${command}`],
        meta: 'exit 0 · loopback',
        startedAt,
        finishedAt: startedAt + 12,
      },
    ]);
  }, []);

  return (
    <TerminalShell theme="nebula" commands={commands} onCommandSubmit={handleSubmit} />
  );
}

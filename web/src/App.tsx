import { useCallback, useRef, useState } from 'react';
import TerminalShell from './components/terminal/TerminalShell';
import type { CommandCardProps } from './components/terminal/types';
import type { CommandEvent } from './components/terminal/commandTracker';
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

export default function App() {
  const [commands, setCommands] = useState<CommandCardProps[]>(LIVE ? [] : DEMO_COMMANDS);
  const transportRef = useRef<TerminalTransport | null>(null);

  /** OSC 133 events from the live session → command cards. */
  const handleCommandEvent = useCallback((event: CommandEvent) => {
    setCommands((prev) => {
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
    });
  }, []);

  const handleTransportReady = useCallback((t: TerminalTransport) => {
    transportRef.current = t;
  }, []);

  /**
   * The input line. Live: write the command into the PTY (the OSC 133 marks
   * then produce its card, exactly as if it were typed in the raw panel).
   * Demo: append a fake card, as before.
   */
  const handleSubmit = useCallback((command: string) => {
    if (LIVE) {
      transportRef.current?.write(`${command}\r`);
      return;
    }
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
    <TerminalShell
      theme="nebula"
      commands={commands}
      onCommandSubmit={handleSubmit}
      onCommandEvent={LIVE ? handleCommandEvent : undefined}
      onTransportReady={LIVE ? handleTransportReady : undefined}
    />
  );
}

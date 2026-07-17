import { useEffect, useRef, useState } from 'react';
import type { FormEvent } from 'react';
import CommandCard from './CommandCard';
import TerminalView from './TerminalView';
import type { CommandCardProps } from './types';
import type { CommandEvent } from './commandTracker';
import type { TerminalTransport } from '../../transport/bridge';

export interface CommandStreamProps {
  commands: CommandCardProps[];
  onCommandSubmit?: (command: string) => void;
  onCommandEvent?: (event: CommandEvent) => void;
  onTransportReady?: (transport: TerminalTransport) => void;
}

/**
 * The center column: a scrolling stream of CommandCards, the raw xterm.js
 * panel, and the command input line at the bottom.
 */
export default function CommandStream({ commands, onCommandSubmit, onCommandEvent, onTransportReady }: CommandStreamProps) {
  const [draft, setDraft] = useState('');
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [commands.length]);

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    const cmd = draft.trim();
    if (cmd.length === 0) return;
    onCommandSubmit?.(cmd);
    setDraft('');
  }

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div ref={scrollRef} className="flex-1 space-y-3 overflow-y-auto p-4">
        {commands.map((c, i) => (
          <CommandCard key={c.id ?? `cmd-${i}`} {...c} />
        ))}

        <TerminalView onCommandEvent={onCommandEvent} onTransportReady={onTransportReady} />
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
          className="flex-1 bg-transparent font-nebula-command text-sm text-nebula-text caret-[#4CE1F7] outline-none transition-colors duration-nebula-fast ease-nebula placeholder:text-nebula-text/25"
          aria-label="Command input"
        />
      </form>
    </main>
  );
}

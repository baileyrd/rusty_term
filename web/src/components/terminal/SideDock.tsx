import type { ReactNode } from 'react';
import type { SideDockProps } from './types';

function MeterBar({ label, value, color }: { label: string; value: number; color: string }) {
  const pct = Math.round(Math.min(Math.max(value, 0), 1) * 100);
  return (
    <div>
      <div className="mb-1 flex justify-between font-nebula-meta text-[11px] text-nebula-text/50">
        <span>{label}</span>
        <span>{pct}%</span>
      </div>
      <div className="h-1.5 overflow-hidden rounded-full bg-white/5">
        <div
          className="h-full rounded-full transition-all duration-nebula-slow ease-nebula"
          style={{ width: `${pct}%`, backgroundColor: color }}
        />
      </div>
    </div>
  );
}

function DockSection({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="rounded-nebula-md border border-white/5 bg-nebula-surface p-3 shadow-nebula-soft">
      <h3 className="mb-2 font-nebula-meta text-[11px] font-semibold uppercase tracking-wider text-nebula-text/40">
        {title}
      </h3>
      {children}
    </section>
  );
}

/**
 * Right-hand side dock: process monitor bars, recent command history, and
 * pinned snippet cards.
 */
export default function SideDock({
  cpu,
  ram,
  recentCommands = [],
  pinnedSnippets = [],
  onSnippetClick,
  onSnippetRemove,
  onRecentCommandClick,
}: SideDockProps) {
  return (
    <aside className="hidden w-64 shrink-0 flex-col gap-3 overflow-y-auto border-l border-white/5 p-3 lg:flex">
      <DockSection title="Processes">
        <div className="flex flex-col gap-2.5">
          <MeterBar label="CPU" value={cpu} color="#4CE1F7" />
          <MeterBar label="RAM" value={ram} color="#F7C14C" />
        </div>
      </DockSection>

      <DockSection title="Recent">
        {recentCommands.length === 0 ? (
          <p className="font-nebula-meta text-xs text-nebula-text/30">No history yet.</p>
        ) : (
          <ul className="flex flex-col gap-1">
            {recentCommands.map((cmd, i) => (
              <li key={`${i}-${cmd}`}>
                <button
                  type="button"
                  onClick={() => onRecentCommandClick?.(cmd)}
                  className="w-full truncate rounded-nebula-sm px-1.5 py-1 text-left font-nebula-command text-xs text-nebula-text/70 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5 hover:text-nebula-text"
                  title={`Run again: ${cmd}`}
                >
                  {cmd}
                </button>
              </li>
            ))}
          </ul>
        )}
      </DockSection>

      <DockSection title="Pinned snippets">
        {pinnedSnippets.length === 0 ? (
          <p className="font-nebula-meta text-xs text-nebula-text/30">Pin a snippet to keep it here.</p>
        ) : (
          <ul className="flex flex-col gap-2">
            {pinnedSnippets.map((s, i) => (
              <li
                key={`${i}-${s.command}`}
                data-testid="pinned-snippet"
                className="group relative rounded-nebula-sm border border-white/5 p-2 transition-colors duration-nebula-fast ease-nebula hover:border-nebula-accent/30"
              >
                <button
                  type="button"
                  onClick={() => onSnippetClick?.(s)}
                  className="block w-full cursor-pointer text-left"
                  title={`Run: ${s.command}`}
                >
                  <p className="mb-1 font-nebula-meta text-[11px] text-nebula-accent2">{s.title}</p>
                  <code className="block truncate font-nebula-command text-xs text-nebula-text/70">
                    {s.command}
                  </code>
                </button>
                {onSnippetRemove && (
                  <button
                    type="button"
                    onClick={() => onSnippetRemove(s)}
                    aria-label={`Unpin ${s.title}`}
                    className="absolute right-1 top-1 hidden rounded-nebula-sm px-1 font-nebula-meta text-[11px] text-nebula-text/40 hover:bg-white/10 hover:text-nebula-error group-hover:block"
                  >
                    ✕
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
      </DockSection>
    </aside>
  );
}

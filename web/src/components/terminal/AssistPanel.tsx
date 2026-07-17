import type { AssistInsight } from '../../assist/heuristics';

const KIND_ACCENT: Record<AssistInsight['kind'], string> = {
  summary: 'border-nebula-accent/30 text-nebula-accent',
  failure: 'border-nebula-error/40 text-nebula-error',
  tip: 'border-nebula-warning/40 text-nebula-warning',
};

export interface AssistPanelProps {
  insights: AssistInsight[];
  onRun?: (command: string) => void;
  onClose: () => void;
}

/**
 * The AI orb's sheet: slides up from the bottom-right (the design system's
 * "modal sheets slide up" rule) with the session insights the local
 * heuristics provider computed. Deliberately labeled as local rules — there
 * is no model behind it, and it doesn't pretend otherwise.
 */
export default function AssistPanel({ insights, onRun, onClose }: AssistPanelProps) {
  return (
    <div
      data-testid="assist-panel"
      role="dialog"
      aria-label="Assist"
      className="fixed bottom-20 right-6 z-20 flex max-h-[60vh] w-80 animate-nebula-fade-in flex-col overflow-hidden rounded-nebula-lg border border-white/10 bg-nebula-bg shadow-nebula-soft"
    >
      <header className="flex items-center justify-between border-b border-white/5 bg-nebula-surface px-4 py-2.5">
        <div>
          <h2 className="font-nebula-meta text-sm font-semibold text-nebula-text">Assist</h2>
          <p className="font-nebula-meta text-[10px] text-nebula-text/40">
            local heuristics · no AI provider connected
          </p>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close assist panel"
          className="rounded-nebula-sm px-2 py-1 font-nebula-meta text-xs text-nebula-text/50 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5 hover:text-nebula-text"
        >
          ✕
        </button>
      </header>

      <div className="flex flex-col gap-2 overflow-y-auto p-3">
        {insights.map((insight) => (
          <section
            key={insight.id}
            data-testid="assist-insight"
            data-kind={insight.kind}
            className={`rounded-nebula-md border bg-nebula-surface p-3 ${KIND_ACCENT[insight.kind]}`}
          >
            <h3 className="mb-1 font-nebula-meta text-xs font-semibold">{insight.title}</h3>
            <p className="font-nebula-meta text-xs leading-relaxed text-nebula-text/70">
              {insight.body}
            </p>
            {insight.suggestedCommand !== undefined && (
              <div className="mt-2 flex items-center gap-2">
                <code className="min-w-0 flex-1 truncate rounded-nebula-sm bg-black/30 px-2 py-1 font-nebula-command text-xs text-nebula-text/80">
                  {insight.suggestedCommand}
                </code>
                {onRun && (
                  <button
                    type="button"
                    onClick={() => onRun(insight.suggestedCommand as string)}
                    className="rounded-nebula-sm border border-nebula-accent/40 px-2 py-1 font-nebula-meta text-[11px] text-nebula-accent transition-colors duration-nebula-fast ease-nebula hover:bg-nebula-accent/10"
                  >
                    run
                  </button>
                )}
                <button
                  type="button"
                  onClick={() => void navigator.clipboard?.writeText(insight.suggestedCommand as string)}
                  className="rounded-nebula-sm border border-white/10 px-2 py-1 font-nebula-meta text-[11px] text-nebula-text/60 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5"
                >
                  copy
                </button>
              </div>
            )}
          </section>
        ))}
      </div>
    </div>
  );
}

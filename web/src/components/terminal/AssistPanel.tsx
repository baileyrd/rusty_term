import { useState } from 'react';
import type { AssistInsight } from '../../assist/heuristics';
import { ASSIST_MODEL } from '../../assist/llmProvider';

const KIND_ACCENT: Record<AssistInsight['kind'], string> = {
  summary: 'border-nebula-accent/30 text-nebula-accent',
  failure: 'border-nebula-error/40 text-nebula-error',
  tip: 'border-nebula-warning/40 text-nebula-warning',
};

/** Where the panel's AI half currently stands. */
export type AiAssistState =
  | { phase: 'disconnected' }
  | { phase: 'loading' }
  | { phase: 'ready'; insights: AssistInsight[] }
  | { phase: 'error'; message: string };

export interface AssistPanelProps {
  insights: AssistInsight[];
  ai: AiAssistState;
  onConnect: (apiKey: string) => void;
  onDisconnect: () => void;
  onRun?: (command: string) => void;
  onClose: () => void;
}

function InsightCard({
  insight,
  source,
  onRun,
}: {
  insight: AssistInsight;
  source: 'local' | 'ai';
  onRun?: (command: string) => void;
}) {
  return (
    <section
      data-testid="assist-insight"
      data-kind={insight.kind}
      data-source={source}
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
  );
}

/** The bottom strip: paste-key form when disconnected, disconnect when not. */
function ConnectionBar({
  ai,
  onConnect,
  onDisconnect,
}: Pick<AssistPanelProps, 'ai' | 'onConnect' | 'onDisconnect'>) {
  const [draftKey, setDraftKey] = useState('');

  if (ai.phase === 'disconnected') {
    return (
      <form
        data-testid="assist-connect"
        className="flex items-center gap-2 border-t border-white/5 bg-nebula-surface px-3 py-2.5"
        onSubmit={(e) => {
          e.preventDefault();
          const key = draftKey.trim();
          if (key.length > 0) onConnect(key);
          setDraftKey('');
        }}
      >
        <input
          type="password"
          value={draftKey}
          onChange={(e) => setDraftKey(e.target.value)}
          placeholder="Anthropic API key…"
          aria-label="Anthropic API key"
          autoComplete="off"
          className="min-w-0 flex-1 rounded-nebula-sm border border-white/10 bg-black/30 px-2 py-1 font-nebula-command text-xs text-nebula-text placeholder:text-nebula-text/30 focus:border-nebula-accent/50 focus:outline-none"
        />
        <button
          type="submit"
          disabled={draftKey.trim().length === 0}
          className="rounded-nebula-sm border border-nebula-accent/40 px-2 py-1 font-nebula-meta text-[11px] text-nebula-accent transition-colors duration-nebula-fast ease-nebula hover:bg-nebula-accent/10 disabled:opacity-40"
        >
          connect
        </button>
      </form>
    );
  }

  return (
    <div className="flex items-center justify-between border-t border-white/5 bg-nebula-surface px-3 py-2">
      <span className="font-nebula-meta text-[10px] text-nebula-text/40">
        key held in sessionStorage only
      </span>
      <button
        type="button"
        data-testid="assist-disconnect"
        onClick={onDisconnect}
        className="rounded-nebula-sm border border-white/10 px-2 py-1 font-nebula-meta text-[11px] text-nebula-text/60 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5 hover:text-nebula-error"
      >
        disconnect
      </button>
    </div>
  );
}

/**
 * The AI orb's sheet: slides up from the bottom-right (the design system's
 * "modal sheets slide up" rule). Always shows the local-heuristics insights;
 * when the user connects an Anthropic API key (session-scoped, never
 * persisted to disk), a Claude-generated section appears above them.
 */
export default function AssistPanel({
  insights,
  ai,
  onConnect,
  onDisconnect,
  onRun,
  onClose,
}: AssistPanelProps) {
  const subtitle =
    ai.phase === 'disconnected'
      ? 'local heuristics · no AI provider connected'
      : `local heuristics + Claude (${ASSIST_MODEL})`;

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
          <p data-testid="assist-provider-label" className="font-nebula-meta text-[10px] text-nebula-text/40">
            {subtitle}
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
        {ai.phase === 'loading' && (
          <p
            data-testid="assist-ai-status"
            className="animate-pulse font-nebula-meta text-xs text-nebula-accent/70"
          >
            Claude is reading the session…
          </p>
        )}
        {ai.phase === 'error' && (
          <p data-testid="assist-ai-status" className="font-nebula-meta text-xs text-nebula-error/80">
            Assist request failed: {ai.message}
          </p>
        )}
        {ai.phase === 'ready' &&
          ai.insights.map((insight) => (
            <InsightCard key={insight.id} insight={insight} source="ai" onRun={onRun} />
          ))}
        {insights.map((insight) => (
          <InsightCard key={insight.id} insight={insight} source="local" onRun={onRun} />
        ))}
      </div>

      <ConnectionBar ai={ai} onConnect={onConnect} onDisconnect={onDisconnect} />
    </div>
  );
}

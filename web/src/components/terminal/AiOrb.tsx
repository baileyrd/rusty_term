import type { AiOrbProps } from './types';

/**
 * Bottom-right AI assistant orb: a pulsing cyan sphere with an unread-hint
 * badge. Purely presentational for now — the assistant itself is a later
 * phase.
 */
export default function AiOrb({ unreadHints = 0, enabled = true, onClick }: AiOrbProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={!enabled}
      aria-label={
        unreadHints > 0 ? `AI assistant, ${unreadHints} unread hints` : 'AI assistant'
      }
      className={`fixed bottom-6 right-6 z-20 flex h-12 w-12 items-center justify-center rounded-full transition-transform duration-nebula-base ease-nebula ${
        enabled
          ? 'animate-nebula-pulse cursor-pointer hover:scale-105'
          : 'cursor-default opacity-40'
      }`}
      style={{
        background:
          'radial-gradient(circle at 35% 30%, rgb(var(--nebula-accent) / 0.95), rgb(var(--nebula-accent) / 0.75) 45%, rgb(var(--nebula-accent) / 0.25) 100%)',
        boxShadow: '0 4px 12px rgba(0,0,0,0.35), 0 0 24px rgb(var(--nebula-accent) / 0.35)',
      }}
    >
      <span className="sr-only">AI assistant</span>
      {unreadHints > 0 && (
        <span className="absolute -right-1 -top-1 flex h-5 min-w-5 items-center justify-center rounded-full bg-nebula-accent2 px-1 font-nebula-meta text-[11px] font-semibold text-black">
          {unreadHints}
        </span>
      )}
    </button>
  );
}

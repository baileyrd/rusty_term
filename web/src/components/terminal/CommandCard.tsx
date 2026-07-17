import type { CommandCardProps, CommandStatus } from './types';

const STATUS_COLOR: Record<CommandStatus, string> = {
  idle: 'text-nebula-text/50',
  running: 'text-nebula-accent',
  success: 'text-nebula-success',
  error: 'text-nebula-error',
};

const STATUS_BORDER: Record<CommandStatus, string> = {
  idle: 'border-white/5',
  running: 'border-nebula-accent/30',
  success: 'border-nebula-success/20',
  error: 'border-nebula-error/30',
};

const STATUS_LABEL: Record<CommandStatus, string> = {
  idle: '·',
  running: '● running',
  success: '✔',
  error: '✘',
};

function formatDuration(startedAt?: number, finishedAt?: number): string | null {
  if (startedAt === undefined || finishedAt === undefined) return null;
  const ms = finishedAt - startedAt;
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

/**
 * A single executed command rendered as a Nebula card: header (prompt glyph +
 * command + status), hairline divider, output block, metadata footer.
 * Fades in over 80ms per the spec.
 */
export default function CommandCard({
  command,
  status,
  output,
  meta,
  startedAt,
  finishedAt,
}: CommandCardProps) {
  const duration = formatDuration(startedAt, finishedAt);

  return (
    <article
      data-testid="command-card"
      data-status={status}
      className={`animate-nebula-fade-in rounded-nebula-md border ${STATUS_BORDER[status]} bg-nebula-surface shadow-nebula-soft transition-colors duration-nebula-base ease-nebula`}
    >
      <header className="flex items-baseline gap-2 px-4 pt-3 pb-2">
        <span className="select-none font-nebula-command text-sm text-nebula-accent2">❯</span>
        <span className="flex-1 truncate font-nebula-command text-sm text-nebula-text">
          {command}
        </span>
        <span
          className={`font-nebula-meta text-xs ${STATUS_COLOR[status]} ${
            status === 'running' ? 'animate-pulse' : ''
          }`}
        >
          {STATUS_LABEL[status]}
        </span>
      </header>

      {output.length > 0 && (
        <>
          <div className="mx-4 border-t border-white/5" />
          <pre className="overflow-x-auto px-4 py-2 font-nebula-output text-[13px] leading-relaxed text-nebula-text/85">
            {output.join('\n')}
          </pre>
        </>
      )}

      {(meta || duration) && (
        <footer className="flex items-center gap-3 px-4 pb-2.5 font-nebula-meta text-[11px] text-nebula-text/40">
          {duration && <span>{duration}</span>}
          {meta && <span>{meta}</span>}
        </footer>
      )}
    </article>
  );
}

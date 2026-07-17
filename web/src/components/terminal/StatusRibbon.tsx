import type { StatusRibbonProps } from './types';

function Sparkline({ samples }: { samples: number[] }) {
  const w = 72;
  const h = 18;
  if (samples.length < 2) {
    return <svg width={w} height={h} aria-hidden="true" />;
  }
  const step = w / (samples.length - 1);
  const points = samples
    .map((v, i) => `${(i * step).toFixed(1)},${(h - Math.min(Math.max(v, 0), 1) * h).toFixed(1)}`)
    .join(' ');
  return (
    <svg width={w} height={h} className="opacity-80" role="img" aria-label="system load">
      <polyline
        points={points}
        fill="none"
        stroke="#4CE1F7"
        strokeWidth="1.5"
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
}

function latencyColor(ms: number): string {
  if (ms < 50) return 'bg-nebula-success';
  if (ms < 150) return 'bg-nebula-warning';
  return 'bg-nebula-error';
}

/**
 * Top status ribbon: system-load sparkline, latency dot, environment pill,
 * and a git branch chip with add/modify/delete stats.
 */
export default function StatusRibbon({
  systemLoad,
  latencyMs,
  environment,
  gitBranch,
  gitStats,
}: StatusRibbonProps) {
  return (
    <div className="flex h-11 shrink-0 items-center gap-4 border-b border-white/5 bg-nebula-surface px-4 font-nebula-meta text-xs shadow-nebula-soft">
      <div className="flex items-center gap-2" title="System load">
        <Sparkline samples={systemLoad} />
      </div>

      <div className="flex items-center gap-1.5" title={`Latency ${latencyMs}ms`}>
        <span className={`h-2 w-2 rounded-full ${latencyColor(latencyMs)}`} />
        <span className="text-nebula-text/60">{latencyMs}ms</span>
      </div>

      <span className="rounded-full border border-nebula-accent/30 px-2.5 py-0.5 text-nebula-accent">
        {environment}
      </span>

      <div className="ml-auto flex items-center gap-2 rounded-nebula-sm border border-white/5 px-2.5 py-1">
        <span className="text-nebula-accent2"></span>
        <span className="font-nebula-command text-nebula-text/90">{gitBranch}</span>
        <span className="text-nebula-success">+{gitStats.added}</span>
        <span className="text-nebula-warning">~{gitStats.modified}</span>
        <span className="text-nebula-error">-{gitStats.deleted}</span>
      </div>
    </div>
  );
}

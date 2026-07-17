import { useEffect, useState, type ReactNode } from 'react';
import { ASSIST_MODEL } from '../../assist/llmProvider';
import { THEME_NAMES, type ThemeName } from '../../theme/tokens';
import type { SnippetItem } from './types';

export interface SettingsSheetProps {
  open: boolean;
  onClose: () => void;
  activeTheme: ThemeName;
  onSetTheme: (theme: ThemeName) => void;
  apiKeyConnected: boolean;
  onConnectAssist: (apiKey: string) => void;
  onDisconnectAssist: () => void;
  snippets: SnippetItem[];
  onClearSnippets: () => void;
}

const THEME_SWATCH: Record<ThemeName, string> = {
  nebula: 'radial-gradient(circle at 30% 30%, #4CE1F7, #0A0A0F 70%)',
  cyberpunk: 'radial-gradient(circle at 30% 30%, #FF2A6D, #0B0014 70%)',
  minimal: 'radial-gradient(circle at 30% 30%, #8FA8C7, #0E0E11 70%)',
};

const SHORTCUTS: [string, string][] = [
  ['Ctrl / Cmd + K', 'Command palette'],
  ['Ctrl / Cmd + Shift + F', 'Search session history'],
  ['Ctrl / Cmd + ,', 'This settings sheet'],
  ['Esc', 'Close the open overlay'],
];

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-2 border-b border-white/5 p-4 last:border-b-0">
      <h3 className="font-nebula-meta text-xs font-semibold uppercase tracking-wide text-nebula-text/40">
        {title}
      </h3>
      {children}
    </section>
  );
}

/**
 * The Ctrl/Cmd+, settings sheet: a single place for cross-cutting
 * preferences the palette otherwise scatters across one-shot actions —
 * the theme picker, the assist connection, pinned-snippet housekeeping —
 * plus a static keyboard-shortcut reference. Same overlay conventions as
 * the palette and search: capture-phase Escape, backdrop dismiss.
 */
export default function SettingsSheet({
  open,
  onClose,
  activeTheme,
  onSetTheme,
  apiKeyConnected,
  onConnectAssist,
  onDisconnectAssist,
  snippets,
  onClearSnippets,
}: SettingsSheetProps) {
  const [draftKey, setDraftKey] = useState('');

  useEffect(() => {
    if (!open) setDraftKey('');
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-30 bg-black/50" onClick={onClose}>
      <div
        data-testid="settings-sheet"
        role="dialog"
        aria-label="Settings"
        onClick={(e) => e.stopPropagation()}
        className="mx-auto mt-[10vh] flex max-h-[76vh] w-[30rem] max-w-[calc(100vw-2rem)] animate-nebula-fade-in flex-col overflow-hidden rounded-nebula-lg border border-white/10 bg-nebula-bg shadow-nebula-soft"
      >
        <header className="flex items-center justify-between border-b border-white/5 bg-nebula-surface px-4 py-2.5">
          <h2 className="font-nebula-meta text-sm font-semibold text-nebula-text">Settings</h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close settings"
            className="rounded-nebula-sm px-2 py-1 font-nebula-meta text-xs text-nebula-text/50 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5 hover:text-nebula-text"
          >
            ✕
          </button>
        </header>

        <div className="overflow-y-auto">
          <Section title="Appearance">
            <div className="flex gap-2">
              {THEME_NAMES.map((name) => (
                <button
                  key={name}
                  type="button"
                  data-testid="settings-theme-option"
                  data-active={name === activeTheme}
                  aria-pressed={name === activeTheme}
                  onClick={() => onSetTheme(name)}
                  className={`flex flex-1 flex-col items-center gap-1.5 rounded-nebula-md border p-2 transition-colors duration-nebula-fast ease-nebula ${
                    name === activeTheme
                      ? 'border-nebula-accent/60 bg-nebula-accent/10'
                      : 'border-white/10 hover:border-white/20'
                  }`}
                >
                  <span
                    className="h-8 w-8 rounded-full border border-white/10"
                    style={{ background: THEME_SWATCH[name] }}
                  />
                  <span className="font-nebula-meta text-[11px] capitalize text-nebula-text/80">
                    {name}
                  </span>
                </button>
              ))}
            </div>
          </Section>

          <Section title="Assist">
            {apiKeyConnected ? (
              <div className="flex items-center justify-between">
                <p className="font-nebula-meta text-xs text-nebula-text/70">
                  Connected · {ASSIST_MODEL}
                </p>
                <button
                  type="button"
                  data-testid="settings-disconnect"
                  onClick={onDisconnectAssist}
                  className="rounded-nebula-sm border border-white/10 px-2 py-1 font-nebula-meta text-[11px] text-nebula-text/60 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5 hover:text-nebula-error"
                >
                  disconnect
                </button>
              </div>
            ) : (
              <form
                data-testid="settings-connect"
                className="flex items-center gap-2"
                onSubmit={(e) => {
                  e.preventDefault();
                  const key = draftKey.trim();
                  if (key.length > 0) onConnectAssist(key);
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
            )}
            <p className="font-nebula-meta text-[10px] text-nebula-text/30">
              Key held in sessionStorage only — never persisted to disk.
            </p>
          </Section>

          <Section title="Pinned snippets">
            <div className="flex items-center justify-between">
              <p className="font-nebula-meta text-xs text-nebula-text/70">
                {snippets.length} pinned
              </p>
              <button
                type="button"
                data-testid="settings-clear-snippets"
                disabled={snippets.length === 0}
                onClick={onClearSnippets}
                className="rounded-nebula-sm border border-white/10 px-2 py-1 font-nebula-meta text-[11px] text-nebula-text/60 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5 hover:text-nebula-error disabled:opacity-40"
              >
                clear all
              </button>
            </div>
          </Section>

          <Section title="Keyboard shortcuts">
            <dl className="flex flex-col gap-1.5">
              {SHORTCUTS.map(([keys, label]) => (
                <div key={keys} className="flex items-center justify-between gap-3">
                  <dt className="font-nebula-meta text-xs text-nebula-text/70">{label}</dt>
                  <dd className="rounded-nebula-sm border border-white/10 bg-black/30 px-2 py-0.5 font-nebula-command text-[11px] text-nebula-text/60">
                    {keys}
                  </dd>
                </div>
              ))}
            </dl>
          </Section>
        </div>
      </div>
    </div>
  );
}

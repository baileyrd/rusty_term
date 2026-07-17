import { useEffect, useRef, useState } from 'react';
import type { AssistInsight } from '../../assist/heuristics';
import { ASSIST_MODEL, type ChatMessage } from '../../assist/llmProvider';
import { parseChatSegments } from '../../assist/chatSegments';

const KIND_ACCENT: Record<AssistInsight['kind'], string> = {
  summary: 'border-nebula-accent/30 text-nebula-accent',
  failure: 'border-nebula-error/40 text-nebula-error',
  tip: 'border-nebula-warning/40 text-nebula-warning',
};

/**
 * Where the panel's AI half currently stands. `streaming` carries the
 * insights completed so far — cards appear one by one as the response
 * streams, then the state settles to `ready` with the validated final set.
 */
export type AiAssistState =
  | { phase: 'disconnected' }
  | { phase: 'loading' }
  | { phase: 'streaming'; insights: AssistInsight[] }
  | { phase: 'ready'; insights: AssistInsight[] }
  | { phase: 'error'; message: string };

/**
 * The chat thread as the shell owns it: past turns (the last assistant
 * message grows while `busy` — it's the streaming reply), plus any error
 * from the most recent send.
 */
export interface ChatState {
  messages: ChatMessage[];
  busy: boolean;
  error?: string;
}

export interface AssistPanelProps {
  insights: AssistInsight[];
  ai: AiAssistState;
  chat: ChatState;
  onChatSend: (text: string) => void;
  /** Run a chat code block in the terminal; unlike `onRun`, keeps the sheet open. */
  onChatRun?: (command: string) => void;
  onConnect: (apiKey: string) => void;
  onDisconnect: () => void;
  onRun?: (command: string) => void;
  onClose: () => void;
  /** Tab to show when the sheet mounts (the palette opens straight to chat). */
  initialTab?: 'insights' | 'chat';
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

/**
 * An assistant turn, with fenced code blocks rendered as runnable command
 * blocks: run submits into the terminal (the sheet stays open — the
 * conversation continues), copy goes to the clipboard.
 */
function AssistantText({ text, onRun }: { text: string; onRun?: (command: string) => void }) {
  return (
    <>
      {parseChatSegments(text).map((seg, i) =>
        seg.type === 'text' ? (
          <span key={i} className="block">
            {seg.content}
          </span>
        ) : (
          <div
            key={i}
            data-testid="assist-chat-code"
            className="my-1.5 overflow-hidden rounded-nebula-sm border border-white/10 bg-black/40"
          >
            <pre className="overflow-x-auto px-2 py-1.5 font-nebula-command text-xs text-nebula-text/90">
              {seg.content}
            </pre>
            <div className="flex items-center justify-end gap-1.5 border-t border-white/5 px-1.5 py-1">
              {seg.lang && (
                <span className="mr-auto font-nebula-meta text-[10px] text-nebula-text/30">
                  {seg.lang}
                </span>
              )}
              {onRun && (
                <button
                  type="button"
                  data-testid="assist-chat-run"
                  onClick={() => onRun(seg.content.trim())}
                  className="rounded-nebula-sm border border-nebula-accent/40 px-2 py-0.5 font-nebula-meta text-[11px] text-nebula-accent transition-colors duration-nebula-fast ease-nebula hover:bg-nebula-accent/10"
                >
                  run
                </button>
              )}
              <button
                type="button"
                onClick={() => void navigator.clipboard?.writeText(seg.content.trim())}
                className="rounded-nebula-sm border border-white/10 px-2 py-0.5 font-nebula-meta text-[11px] text-nebula-text/60 transition-colors duration-nebula-fast ease-nebula hover:bg-white/5"
              >
                copy
              </button>
            </div>
          </div>
        ),
      )}
    </>
  );
}

/** The Chat tab: thread of turns + input line. Needs a connected key. */
function ChatView({
  chat,
  connected,
  onChatSend,
  onRun,
}: {
  chat: ChatState;
  connected: boolean;
  onChatSend: (text: string) => void;
  onRun?: (command: string) => void;
}) {
  const [draft, setDraft] = useState('');
  const logRef = useRef<HTMLDivElement>(null);

  // Keep the newest turn in view as replies stream in.
  useEffect(() => {
    const log = logRef.current;
    if (log) log.scrollTop = log.scrollHeight;
  }, [chat.messages]);

  if (!connected) {
    return (
      <p className="p-4 font-nebula-meta text-xs text-nebula-text/50">
        Connect an Anthropic API key below to chat with Claude about this
        session.
      </p>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div ref={logRef} data-testid="assist-chat-log" className="flex flex-col gap-2 overflow-y-auto p-3">
        {chat.messages.length === 0 && (
          <p className="font-nebula-meta text-xs text-nebula-text/40">
            Ask about the session — failures, next steps, what a command did.
            Claude sees the recent command cards.
          </p>
        )}
        {chat.messages.map((m, i) => (
          <div
            key={i}
            data-testid="assist-chat-message"
            data-role={m.role}
            className={`max-w-[90%] whitespace-pre-wrap rounded-nebula-md border p-2.5 font-nebula-meta text-xs leading-relaxed ${
              m.role === 'user'
                ? 'self-end border-nebula-accent/30 bg-nebula-accent/10 text-nebula-text'
                : 'self-start border-white/10 bg-nebula-surface text-nebula-text/85'
            }`}
          >
            {m.role === 'assistant' ? <AssistantText text={m.text} onRun={onRun} /> : m.text}
            {chat.busy && m.role === 'assistant' && i === chat.messages.length - 1 && (
              <span className="ml-1 inline-block h-3 w-1.5 animate-pulse bg-nebula-accent align-text-bottom" />
            )}
          </div>
        ))}
        {chat.error !== undefined && (
          <p data-testid="assist-chat-error" className="font-nebula-meta text-xs text-nebula-error/80">
            Chat request failed: {chat.error}
          </p>
        )}
      </div>
      <form
        className="flex items-center gap-2 border-t border-white/5 px-3 py-2"
        onSubmit={(e) => {
          e.preventDefault();
          const text = draft.trim();
          if (text.length === 0 || chat.busy) return;
          onChatSend(text);
          setDraft('');
        }}
      >
        <input
          type="text"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder={chat.busy ? 'Claude is replying…' : 'Ask about this session…'}
          aria-label="Chat message"
          data-testid="assist-chat-input"
          disabled={chat.busy}
          className="min-w-0 flex-1 rounded-nebula-sm border border-white/10 bg-black/30 px-2 py-1 font-nebula-command text-xs text-nebula-text placeholder:text-nebula-text/30 focus:border-nebula-accent/50 focus:outline-none disabled:opacity-50"
        />
        <button
          type="submit"
          data-testid="assist-chat-send"
          disabled={chat.busy || draft.trim().length === 0}
          className="rounded-nebula-sm border border-nebula-accent/40 px-2 py-1 font-nebula-meta text-[11px] text-nebula-accent transition-colors duration-nebula-fast ease-nebula hover:bg-nebula-accent/10 disabled:opacity-40"
        >
          send
        </button>
      </form>
    </div>
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
 * "modal sheets slide up" rule). Two tabs: *insights* always shows the
 * local-heuristics cards, with a Claude-generated section above them once
 * an Anthropic API key is connected (session-scoped, never persisted to
 * disk); *chat* is a streaming conversation with Claude about the session,
 * available with the same key.
 */
export default function AssistPanel({
  insights,
  ai,
  chat,
  onChatSend,
  onChatRun,
  onConnect,
  onDisconnect,
  onRun,
  onClose,
  initialTab = 'insights',
}: AssistPanelProps) {
  const [tab, setTab] = useState<'insights' | 'chat'>(initialTab);
  // The palette can retarget an already-open sheet ("Open assist chat").
  useEffect(() => setTab(initialTab), [initialTab]);
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

      <nav className="flex border-b border-white/5 bg-nebula-surface/50">
        {(['insights', 'chat'] as const).map((t) => (
          <button
            key={t}
            type="button"
            data-testid={`assist-tab-${t}`}
            aria-selected={tab === t}
            onClick={() => setTab(t)}
            className={`px-4 py-1.5 font-nebula-meta text-xs transition-colors duration-nebula-fast ease-nebula ${
              tab === t
                ? 'border-b-2 border-nebula-accent text-nebula-accent'
                : 'text-nebula-text/50 hover:text-nebula-text'
            }`}
          >
            {t}
          </button>
        ))}
      </nav>

      {tab === 'chat' ? (
        <ChatView
          chat={chat}
          connected={ai.phase !== 'disconnected'}
          onChatSend={onChatSend}
          onRun={onChatRun}
        />
      ) : (
      <div className="flex flex-col gap-2 overflow-y-auto p-3">
        {(ai.phase === 'loading' || ai.phase === 'streaming') && (
          <p
            data-testid="assist-ai-status"
            data-phase={ai.phase}
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
        {(ai.phase === 'ready' || ai.phase === 'streaming') &&
          ai.insights.map((insight) => (
            <InsightCard key={insight.id} insight={insight} source="ai" onRun={onRun} />
          ))}
        {insights.map((insight) => (
          <InsightCard key={insight.id} insight={insight} source="local" onRun={onRun} />
        ))}
      </div>
      )}

      <ConnectionBar ai={ai} onConnect={onConnect} onDisconnect={onDisconnect} />
    </div>
  );
}

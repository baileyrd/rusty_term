/**
 * The Claude-backed assist provider: the "tomorrow's LLM" half of the
 * `AssistProvider` socket that `heuristics.ts` promises. Given an API key it
 * sends the session's command cards to the Messages API and gets back the
 * same `AssistInsight` shape the local rules produce, constrained by a JSON
 * schema so the panel never has to parse prose.
 *
 * Browser use is deliberate and scoped: the key lives in sessionStorage only
 * (never localStorage, never the bundle), the SDK requires the explicit
 * `dangerouslyAllowBrowser` opt-in, and the UI labels the connection state.
 * For tests, `nebula.assistBaseUrl` (sessionStorage) points the SDK at a
 * mock Messages endpoint.
 */

import type Anthropic from '@anthropic-ai/sdk';
import type { CommandCardProps } from '../components/terminal/types';
import type { AssistInsight } from './heuristics';

/** sessionStorage key for the user's Anthropic API key. */
export const API_KEY_STORAGE = 'nebula.assistApiKey';
/** sessionStorage key for a Messages API base-URL override (E2E mocks). */
export const BASE_URL_STORAGE = 'nebula.assistBaseUrl';

export const ASSIST_MODEL = 'claude-opus-4-8';

/**
 * The async twin of `AssistProvider` — network calls can't be sync. The
 * response streams: `onPartial` fires with the insights completed so far
 * (a fresh array each time, ids assigned), and the returned promise
 * resolves with the final validated set.
 */
export interface AsyncAssistProvider {
  analyze(
    commands: CommandCardProps[],
    onPartial?: (insights: AssistInsight[]) => void,
  ): Promise<AssistInsight[]>;
}

export function loadApiKey(): string | null {
  try {
    return sessionStorage.getItem(API_KEY_STORAGE);
  } catch {
    return null;
  }
}

export function storeApiKey(key: string | null): void {
  try {
    if (key === null) sessionStorage.removeItem(API_KEY_STORAGE);
    else sessionStorage.setItem(API_KEY_STORAGE, key);
  } catch {
    // Blocked storage: the connection just won't survive a reload.
  }
}

function baseUrlOverride(): string | undefined {
  try {
    return sessionStorage.getItem(BASE_URL_STORAGE) ?? undefined;
  } catch {
    return undefined;
  }
}

/** Response shape we ask the model for (top level must be an object). */
const INSIGHTS_SCHEMA = {
  type: 'object',
  properties: {
    insights: {
      type: 'array',
      maxItems: 5,
      items: {
        type: 'object',
        properties: {
          kind: { type: 'string', enum: ['summary', 'failure', 'tip'] },
          title: { type: 'string' },
          body: { type: 'string' },
          suggestedCommand: { type: 'string' },
        },
        required: ['kind', 'title', 'body'],
        additionalProperties: false,
      },
    },
  },
  required: ['insights'],
  additionalProperties: false,
} as const;

const SYSTEM_PROMPT = [
  'You are the assist panel of Nebula, a terminal emulator. You receive the',
  'recent command history of a live shell session (commands, exit status,',
  'truncated output) and return at most five short, concrete insights.',
  'Kinds: "summary" (one, first: what the session is doing), "failure"',
  '(diagnose a failed command from its actual output), "tip" (a pattern or',
  'next step worth pointing out). Include suggestedCommand only when a',
  'single safe shell command directly helps; never suggest destructive',
  'commands. Titles under 8 words; bodies one to two sentences.',
].join(' ');

/** How much session history a single analyze() call ships to the model. */
const MAX_COMMANDS = 20;
const MAX_OUTPUT_LINES = 30;

function sessionPayload(commands: CommandCardProps[]): string {
  const cards = commands.slice(-MAX_COMMANDS).map((c) => ({
    command: c.command,
    status: c.status,
    meta: c.meta,
    durationMs:
      c.startedAt !== undefined && c.finishedAt !== undefined
        ? c.finishedAt - c.startedAt
        : undefined,
    output: c.output.slice(-MAX_OUTPUT_LINES),
  }));
  return JSON.stringify({ commands: cards }, null, 1);
}

function isInsight(v: unknown): v is Omit<AssistInsight, 'id'> {
  return (
    typeof v === 'object' &&
    v !== null &&
    ['summary', 'failure', 'tip'].includes((v as AssistInsight).kind) &&
    typeof (v as AssistInsight).title === 'string' &&
    typeof (v as AssistInsight).body === 'string'
  );
}

/**
 * Extract every *completed* insight object from a partial response.
 *
 * The model streams `{"insights": [ {...}, {...}, ... ]}` as text deltas
 * that can cut off anywhere — mid-string, mid-object. Rather than JSON.parse
 * the (invalid) prefix, scan it: find the array, then walk it with plain
 * string/escape/brace bookkeeping and parse each element the moment its
 * closing brace arrives. Structured output guarantees the shape, so a
 * completed element is always a parseable insight.
 */
export function parsePartialInsights(text: string): Omit<AssistInsight, 'id'>[] {
  const arrayStart = text.indexOf('[', text.indexOf('"insights"'));
  if (text.indexOf('"insights"') === -1 || arrayStart === -1) return [];

  const insights: Omit<AssistInsight, 'id'>[] = [];
  let depth = 0;
  let inString = false;
  let escaped = false;
  let objectStart = -1;
  for (let i = arrayStart + 1; i < text.length; i++) {
    const ch = text[i];
    if (inString) {
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === '"') inString = false;
      continue;
    }
    if (ch === '"') inString = true;
    else if (ch === '{') {
      if (depth === 0) objectStart = i;
      depth++;
    } else if (ch === '}') {
      depth--;
      if (depth === 0 && objectStart !== -1) {
        try {
          const candidate: unknown = JSON.parse(text.slice(objectStart, i + 1));
          if (isInsight(candidate)) insights.push(candidate);
        } catch {
          // A malformed element (shouldn't happen under structured output):
          // skip it rather than lose the stream.
        }
        objectStart = -1;
      }
    } else if (ch === ']' && depth === 0) break;
  }
  return insights;
}

function withIds(insights: Omit<AssistInsight, 'id'>[]): AssistInsight[] {
  return insights.map((insight, i) => ({ ...insight, id: `ai-${i}-${insight.title}` }));
}

function isInsightArray(value: unknown): value is Omit<AssistInsight, 'id'>[] {
  return Array.isArray(value) && value.every(isInsight);
}

/**
 * A provider bound to one API key. Each analyze() is a single streaming
 * Messages call. The SDK itself is imported lazily so its ~300 kB never
 * loads for users who never connect a key.
 */
export function createLlmProvider(apiKey: string): AsyncAssistProvider {
  const clientPromise = import('@anthropic-ai/sdk').then(
    ({ default: AnthropicSdk }) =>
      new AnthropicSdk({
        apiKey,
        baseURL: baseUrlOverride(),
        // The key is user-supplied at runtime and session-scoped; this is
        // the SDK's required acknowledgment for running in a browser at all.
        dangerouslyAllowBrowser: true,
      }),
  );

  return {
    async analyze(
      commands: CommandCardProps[],
      onPartial?: (insights: AssistInsight[]) => void,
    ): Promise<AssistInsight[]> {
      const client = await clientPromise;
      const stream = client.messages.stream({
        model: ASSIST_MODEL,
        max_tokens: 2048,
        thinking: { type: 'adaptive' },
        output_config: { format: { type: 'json_schema', schema: INSIGHTS_SCHEMA } },
        system: SYSTEM_PROMPT,
        messages: [
          {
            role: 'user',
            content: `Recent session history (newest last):\n${sessionPayload(commands)}`,
          },
        ],
      });

      // Surface each insight the moment its closing brace streams in.
      let emitted = 0;
      if (onPartial) {
        stream.on('text', (_delta, snapshot) => {
          const done = parsePartialInsights(snapshot);
          if (done.length > emitted) {
            emitted = done.length;
            onPartial(withIds(done));
          }
        });
      }

      const response = await stream.finalMessage();
      const text = response.content
        .filter((block): block is Anthropic.TextBlock => block.type === 'text')
        .map((block) => block.text)
        .join('');
      const parsed = JSON.parse(text) as { insights?: unknown };
      if (!isInsightArray(parsed.insights)) {
        throw new Error('Assist response did not match the insight schema');
      }
      return withIds(parsed.insights);
    },
  };
}

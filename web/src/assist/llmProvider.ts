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

/** The async twin of `AssistProvider` — network calls can't be sync. */
export interface AsyncAssistProvider {
  analyze(commands: CommandCardProps[]): Promise<AssistInsight[]>;
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

function isInsightArray(value: unknown): value is Omit<AssistInsight, 'id'>[] {
  return (
    Array.isArray(value) &&
    value.every(
      (v) =>
        typeof v === 'object' &&
        v !== null &&
        ['summary', 'failure', 'tip'].includes((v as AssistInsight).kind) &&
        typeof (v as AssistInsight).title === 'string' &&
        typeof (v as AssistInsight).body === 'string',
    )
  );
}

/**
 * A provider bound to one API key. Each analyze() is a single Messages call.
 * The SDK itself is imported lazily so its ~300 kB never loads for users
 * who never connect a key.
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
    async analyze(commands: CommandCardProps[]): Promise<AssistInsight[]> {
      const client = await clientPromise;
      const response = await client.messages.create({
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

      const text = response.content
        .filter((block): block is Anthropic.TextBlock => block.type === 'text')
        .map((block) => block.text)
        .join('');
      const parsed = JSON.parse(text) as { insights?: unknown };
      if (!isInsightArray(parsed.insights)) {
        throw new Error('Assist response did not match the insight schema');
      }
      return parsed.insights.map((insight, i) => ({
        ...insight,
        id: `ai-${i}-${insight.title}`,
      }));
    },
  };
}

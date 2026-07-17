/**
 * Split an assistant chat reply into text and fenced-code segments so the
 * chat view can render ```code blocks``` as runnable command blocks.
 *
 * Built for streaming: the reply arrives token-by-token, so an opening
 * fence whose closer hasn't streamed in yet is returned as a (growing)
 * code segment — the block renders as code from its first line instead of
 * flashing as prose and re-wrapping when the closer lands.
 */

export interface ChatSegment {
  type: 'text' | 'code';
  content: string;
  /** The fence's info string (e.g. "sh"), when one was given. */
  lang?: string;
}

const FENCE = /^\s*```(.*)$/;

export function parseChatSegments(text: string): ChatSegment[] {
  const segments: ChatSegment[] = [];
  let buffer: string[] = [];
  let inCode = false;
  let lang: string | undefined;

  const flush = (type: ChatSegment['type']) => {
    const content = buffer.join('\n');
    // Drop whitespace-only text between blocks, keep blank lines inside code.
    if (type === 'code' ? content.length > 0 : content.trim().length > 0) {
      segments.push({ type, content, ...(type === 'code' && lang ? { lang } : {}) });
    }
    buffer = [];
  };

  for (const line of text.split('\n')) {
    const fence = line.match(FENCE);
    if (fence) {
      if (inCode) {
        flush('code');
        inCode = false;
        lang = undefined;
      } else {
        flush('text');
        inCode = true;
        lang = fence[1].trim() || undefined;
      }
      continue;
    }
    buffer.push(line);
  }
  flush(inCode ? 'code' : 'text');
  return segments;
}

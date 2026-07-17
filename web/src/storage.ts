/**
 * A shared, versioned load/save pair for the workspace's structured
 * localStorage keys (pinned snippets, pane layouts, the session/tab/card
 * workspace) — the ones whose shape can plausibly change across a future
 * feature. Each caller picks its own `version`; a stored envelope whose
 * version doesn't match the code reading it is treated as absent rather
 * than guessed at, so a shape change is a clean reset for that one key
 * instead of a runtime validator quietly rejecting fields it doesn't
 * recognize (or worse, half-accepting a shape it wasn't written for).
 *
 * Simple single-value keys (the theme name, the assist API key) don't use
 * this — a bare string has no shape to drift, so the plain
 * `storage.getItem`/`setItem` each already used is simplest as-is.
 */

interface StorageEnvelope<T> {
  v: number;
  data: T;
}

/**
 * Read and validate a versioned JSON value. `null` on any failure —
 * missing key, corrupt JSON, a version mismatch, or a shape `isValid`
 * rejects — so every caller's existing "fall back to defaults" behavior
 * is unchanged; this just centralizes *how* that decision gets made.
 */
export function loadJson<T>(
  storageArea: Storage,
  key: string,
  version: number,
  isValid: (data: unknown) => data is T,
): T | null {
  try {
    const raw = storageArea.getItem(key);
    if (raw === null) return null;
    const parsed: unknown = JSON.parse(raw);
    if (
      typeof parsed !== 'object' ||
      parsed === null ||
      !('v' in parsed) ||
      !('data' in parsed) ||
      (parsed as { v: unknown }).v !== version
    ) {
      return null;
    }
    const { data } = parsed as StorageEnvelope<unknown>;
    return isValid(data) ? data : null;
  } catch {
    return null;
  }
}

/** Write a versioned JSON value. Silently a no-op if storage is full/blocked. */
export function saveJson<T>(storageArea: Storage, key: string, version: number, data: T): void {
  try {
    storageArea.setItem(key, JSON.stringify({ v: version, data } satisfies StorageEnvelope<T>));
  } catch {
    // Storage full/blocked: this key simply doesn't persist this session.
  }
}

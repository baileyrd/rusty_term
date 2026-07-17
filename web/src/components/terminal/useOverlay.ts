import { useEffect } from 'react';

/**
 * The capture-phase Escape handler every modal overlay (palette, search,
 * settings) needs: closes regardless of where focus is (it can land before
 * an input's deferred focus, or after focus wandered), and runs in the
 * capture phase so a focused xterm instance never sees the keypress while
 * the overlay is open.
 */
export function useOverlayEscape(open: boolean, onClose: () => void): void {
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
}

/**
 * The open/close lifecycle every overlay needs around its own local state
 * (a query string, a cursor index, a draft field, ...): `onOpen` runs when
 * the overlay mounts open (typically focusing an input), `onClose` runs
 * when it closes (typically resetting that local state).
 *
 * The reset deliberately happens on *close*, not on open: this effect runs
 * after paint, so a reset on open would fire after the overlay has already
 * rendered and could wipe input typed in the first frames post-mount (a
 * real race this fixed three times over before being extracted here).
 */
export function useOverlayLifecycle(
  open: boolean,
  { onOpen, onClose }: { onOpen?: () => void; onClose?: () => void },
): void {
  useEffect(() => {
    if (open) {
      onOpen?.();
    } else {
      onClose?.();
    }
    // onOpen/onClose are expected to be stable (or the caller doesn't mind
    // re-running) — keying only on `open` mirrors what all three overlays
    // did before this was extracted.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);
}

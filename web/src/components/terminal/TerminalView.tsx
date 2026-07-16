import { useEffect, useRef } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { LoopbackTransport, type TerminalTransport } from '../../transport/bridge';
import { ansiPalette, fonts } from '../../theme/tokens';

export interface TerminalViewProps {
  /**
   * Transport backing this terminal. Defaults to the offline
   * `LoopbackTransport`; pass a websocket transport to talk to a real
   * rusty_term PTY bridge.
   */
  transport?: TerminalTransport;
  /** Bridge endpoint handed to `transport.connect`. Ignored by the loopback. */
  url?: string;
}

/**
 * Raw terminal panel: a real xterm.js instance themed with the Nebula ANSI
 * palette, kept fitted to its container and wired to a `TerminalTransport`.
 */
export default function TerminalView({ transport, url = 'ws://localhost:9090/pty' }: TerminalViewProps) {
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: fonts.output.join(', '),
      theme: { ...ansiPalette },
      scrollback: 2000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);
    fit.fit();

    const t = transport ?? new LoopbackTransport();
    const ownsTransport = transport === undefined;

    const offData = t.onData((data) => term.write(data));
    const offExit = t.onExit((code) => {
      term.write(`\r\n\x1b[38;2;232;232;240m[session exited with code ${code}]\x1b[0m\r\n`);
    });
    const onInput = term.onData((data) => t.write(data));

    void t.connect(url).then(() => {
      t.resize(term.cols, term.rows);
    });

    const resizeObserver = new ResizeObserver(() => {
      fit.fit();
      t.resize(term.cols, term.rows);
    });
    resizeObserver.observe(el);

    return () => {
      resizeObserver.disconnect();
      onInput.dispose();
      offData();
      offExit();
      if (ownsTransport) t.dispose();
      term.dispose();
    };
  }, [transport, url]);

  return (
    <div className="overflow-hidden rounded-nebula-md border border-white/5 bg-nebula-bg shadow-nebula-soft">
      <div className="flex items-center gap-2 border-b border-white/5 px-3 py-1.5 font-nebula-meta text-[11px] text-nebula-text/40">
        <span className="h-2 w-2 rounded-full bg-nebula-accent/60" />
        raw terminal
      </div>
      <div ref={containerRef} className="h-56 p-2" />
    </div>
  );
}

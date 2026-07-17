import { useEffect, useRef } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { transportFromLocation, type TerminalTransport } from '../../transport/bridge';
import { attachCommandTracker, type CommandEvent } from './commandTracker';
import { ansiPalette, fonts } from '../../theme/tokens';

export interface TerminalViewProps {
  /**
   * Transport backing this terminal. Defaults to the offline
   * `LoopbackTransport`; pass a websocket transport to talk to a real
   * rusty_term PTY bridge.
   */
  transport?: TerminalTransport;
  /**
   * Bridge endpoint handed to `transport.connect`. When neither prop is
   * given, both come from the page URL: `?ws[=ws://host:port]` selects the
   * live `WebSocketTransport` against a running `rusty_term_web_bridge`,
   * and no parameter selects the offline loopback demo.
   */
  url?: string;
  /**
   * Structured command events parsed from the session's OSC 133 shell
   * integration (see `commandTracker.ts`) — how the command-card stream is
   * fed by a live shell. Never fires when the shell doesn't emit the marks.
   */
  onCommandEvent?: (event: CommandEvent) => void;
  /**
   * Called once the transport is connected and the session started, so the
   * page's input line can write into the same PTY the panel shows.
   */
  onTransportReady?: (transport: TerminalTransport) => void;
}

/**
 * Raw terminal panel: a real xterm.js instance themed with the Nebula ANSI
 * palette, kept fitted to its container and wired to a `TerminalTransport`.
 */
export default function TerminalView({
  transport,
  url,
  onCommandEvent,
  onTransportReady,
}: TerminalViewProps) {
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

    const fallback = transport === undefined ? transportFromLocation(window.location.search) : null;
    const t = transport ?? fallback!.transport;
    const connectUrl = url ?? fallback?.url ?? '';
    const ownsTransport = transport === undefined;

    const offData = t.onData((data) => term.write(data));
    const offExit = t.onExit((code) => {
      term.write(`\r\n\x1b[38;2;232;232;240m[session exited with code ${code}]\x1b[0m\r\n`);
    });
    const onInput = term.onData((data) => t.write(data));
    const tracker = onCommandEvent ? attachCommandTracker(term, onCommandEvent) : null;

    t.connect(connectUrl)
      .then(() => {
        t.resize(term.cols, term.rows);
        onTransportReady?.(t);
      })
      .catch((e: unknown) => {
        term.write(
          `\x1b[38;2;255;95;95m${e instanceof Error ? e.message : String(e)}\x1b[0m\r\n` +
            'Start the bridge with: cargo run --features web-bridge --bin rusty_term_web_bridge\r\n',
        );
      });

    const resizeObserver = new ResizeObserver(() => {
      fit.fit();
      t.resize(term.cols, term.rows);
    });
    resizeObserver.observe(el);

    return () => {
      resizeObserver.disconnect();
      tracker?.dispose();
      onInput.dispose();
      offData();
      offExit();
      if (ownsTransport) t.dispose();
      term.dispose();
    };
  }, [transport, url, onCommandEvent, onTransportReady]);

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

/**
 * Transport layer for the Nebula web frontend.
 *
 * The real deployment plugs a websocket PTY bridge into rusty_term: a small
 * Rust binary that spawns a shell through the repo's `Backend::spawn_shell`
 * (see `src/backend/mod.rs`) and shuttles bytes over a websocket. This
 * interface mirrors that shape — the terminal-emulator side of the repo's
 * `BackendHandle` (write / resize / read-as-events / exit) — so the UI code
 * never needs to know whether it is talking to a demo loopback or a live PTY.
 */

export type DataListener = (data: string) => void;
export type ExitListener = (code: number) => void;
export type Unsubscribe = () => void;

export interface TerminalTransport {
  /**
   * Open the transport. `url` is a `ws://` / `wss://` endpoint for the real
   * bridge; demo implementations may ignore it. Resolves once the session is
   * ready to accept writes.
   */
  connect(url: string): Promise<void>;

  /** Send user input (keystrokes, pastes) to the PTY. */
  write(data: string): void;

  /** Inform the PTY of a new grid size, mirroring `BackendHandle::set_winsize`. */
  resize(cols: number, rows: number): void;

  /** Subscribe to output from the PTY. Returns an unsubscribe function. */
  onData(listener: DataListener): Unsubscribe;

  /**
   * Subscribe to session exit, mirroring `BackendHandle::reap_exit_status`
   * (child exit code, or 128+signal on a signal death).
   */
  onExit(listener: ExitListener): Unsubscribe;

  /** Tear down the transport and release resources. */
  dispose(): void;

  /**
   * Structured session stats, when the transport has a side-channel for
   * them (the websocket bridge pushes `stats <json>` every 2s; the
   * loopback has none). Latency comes from the transport's own ping loop.
   */
  onStats?(listener: StatsListener): Unsubscribe;

  /**
   * Relay the shell's OSC 7 working directory (already decoded to a plain
   * path) so the stats side-channel can report git facts for it.
   */
  noteCwd?(path: string): void;
}

/** One `stats` push from the bridge, plus the measured round-trip latency. */
export interface LiveStats {
  /** 1-min load / cores, `0..` (may exceed 1); `null` off-Linux. */
  load: number | null;
  /** Memory in use, `0..1`; `null` off-Linux. */
  mem: number | null;
  cwd: string | null;
  branch: string | null;
  git: { added: number; modified: number; deleted: number };
  /** Application-level ping RTT in ms; `null` until the first pong. */
  latencyMs: number | null;
}

export type StatsListener = (stats: LiveStats) => void;

/**
 * Offline demo transport: echoes keystrokes locally with just enough line
 * discipline (echo, backspace, CR→CRLF, a fake prompt) for the raw xterm.js
 * panel to feel alive without a backend. Replace with `WebSocketTransport`
 * once the Rust bridge exists.
 */
export class LoopbackTransport implements TerminalTransport {
  private dataListeners = new Set<DataListener>();
  private exitListeners = new Set<ExitListener>();
  private line = '';
  private connected = false;
  private cols = 80;
  private rows = 24;

  private static PROMPT = '\x1b[38;2;76;225;247mrusty_term\x1b[0m \x1b[38;2;247;193;76m❯\x1b[0m ';

  async connect(_url: string): Promise<void> {
    this.connected = true;
    this.emit(
      '\x1b[38;2;232;232;240mNebula loopback transport — offline demo. ' +
        'Type; input is echoed locally.\x1b[0m\r\n\r\n',
    );
    this.emit(LoopbackTransport.PROMPT);
  }

  write(data: string): void {
    if (!this.connected) return;
    for (const ch of data) {
      if (ch === '\r' || ch === '\n') {
        this.emit('\r\n');
        this.runFakeCommand(this.line.trim());
        this.line = '';
        this.emit(LoopbackTransport.PROMPT);
      } else if (ch === '\x7f' || ch === '\b') {
        if (this.line.length > 0) {
          this.line = this.line.slice(0, -1);
          this.emit('\b \b');
        }
      } else if (ch === '\x03') {
        // ^C — abandon the current line, like a shell would.
        this.emit('^C\r\n');
        this.line = '';
        this.emit(LoopbackTransport.PROMPT);
      } else if (ch >= ' ') {
        this.line += ch;
        this.emit(ch);
      }
    }
  }

  resize(cols: number, rows: number): void {
    this.cols = cols;
    this.rows = rows;
  }

  onData(listener: DataListener): Unsubscribe {
    this.dataListeners.add(listener);
    return () => this.dataListeners.delete(listener);
  }

  onExit(listener: ExitListener): Unsubscribe {
    this.exitListeners.add(listener);
    return () => this.exitListeners.delete(listener);
  }

  dispose(): void {
    this.connected = false;
    this.dataListeners.clear();
    this.exitListeners.clear();
  }

  private emit(data: string): void {
    for (const l of this.dataListeners) l(data);
  }

  private runFakeCommand(cmd: string): void {
    if (cmd.length === 0) return;
    switch (cmd) {
      case 'help':
        this.emit(
          'loopback demo commands: help, size, clear, exit\r\n' +
            'anything else is echoed back.\r\n',
        );
        break;
      case 'size':
        this.emit(`${this.cols}x${this.rows}\r\n`);
        break;
      case 'clear':
        this.emit('\x1b[2J\x1b[H');
        break;
      case 'exit':
        this.emit('\x1b[38;2;255;95;95mloopback session closed.\x1b[0m\r\n');
        this.connected = false;
        for (const l of this.exitListeners) l(0);
        break;
      default:
        this.emit(`\x1b[38;2;76;247;162m${cmd}\x1b[0m\r\n`);
        break;
    }
  }
}

/**
 * Live transport: a browser `WebSocket` speaking the rusty_term web bridge's
 * protocol (`src/web_bridge` in the repo root, binary
 * `rusty_term_web_bridge`, built with `cargo build --features web-bridge`):
 *
 * - text frames carry control — we send `start <cols> <rows>` once and
 *   `resize <cols> <rows>` afterwards; the bridge sends `exit <code>` when
 *   the shell exits;
 * - binary frames carry raw PTY bytes in both directions.
 *
 * The bridge spawns the shell on the first `start`, so the socket is opened
 * by `connect()` but the session begins on the first `resize()` call — which
 * `TerminalView` issues as soon as `connect()` resolves, with the fitted
 * grid size.
 */
export class WebSocketTransport implements TerminalTransport {
  private dataListeners = new Set<DataListener>();
  private exitListeners = new Set<ExitListener>();
  private statsListeners = new Set<StatsListener>();
  private socket: WebSocket | null = null;
  private started = false;
  private decoder = new TextDecoder();
  private encoder = new TextEncoder();
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private pingSentAt = 0;
  private latencyMs: number | null = null;

  connect(url: string): Promise<void> {
    return new Promise((resolve, reject) => {
      const socket = new WebSocket(url);
      socket.binaryType = 'arraybuffer';
      socket.onopen = () => {
        // App-level RTT probe (browsers can't send WS Ping frames): one
        // in-flight ping at a time, every 5s.
        this.pingTimer = setInterval(() => {
          if (socket.readyState === WebSocket.OPEN && this.pingSentAt === 0) {
            this.pingSentAt = performance.now();
            socket.send(`ping ${Date.now()}`);
          }
        }, 5000);
        resolve();
      };
      socket.onerror = () => reject(new Error(`bridge connection failed: ${url}`));
      socket.onmessage = (ev: MessageEvent) => {
        if (typeof ev.data === 'string') {
          this.handleControl(ev.data);
          return;
        }
        // Streaming-decode so a UTF-8 sequence split across frames survives.
        const text = this.decoder.decode(ev.data as ArrayBuffer, { stream: true });
        for (const l of this.dataListeners) l(text);
      };
      socket.onclose = () => {
        if (this.pingTimer !== null) clearInterval(this.pingTimer);
        this.pingTimer = null;
        this.socket = null;
      };
      this.socket = socket;
    });
  }

  write(data: string): void {
    if (this.socket?.readyState === WebSocket.OPEN && this.started) {
      this.socket.send(this.encoder.encode(data));
    }
  }

  resize(cols: number, rows: number): void {
    if (this.socket?.readyState !== WebSocket.OPEN) return;
    // The first size we learn doubles as the session start.
    this.socket.send(`${this.started ? 'resize' : 'start'} ${cols} ${rows}`);
    this.started = true;
  }

  onData(listener: DataListener): Unsubscribe {
    this.dataListeners.add(listener);
    return () => this.dataListeners.delete(listener);
  }

  onExit(listener: ExitListener): Unsubscribe {
    this.exitListeners.add(listener);
    return () => this.exitListeners.delete(listener);
  }

  onStats(listener: StatsListener): Unsubscribe {
    this.statsListeners.add(listener);
    return () => this.statsListeners.delete(listener);
  }

  noteCwd(path: string): void {
    if (this.socket?.readyState === WebSocket.OPEN) {
      this.socket.send(`cwd ${path}`);
    }
  }

  /** Route a control (text) frame: `exit`, `pong`, or `stats <json>`. */
  private handleControl(data: string): void {
    const space = data.indexOf(' ');
    const verb = space < 0 ? data : data.slice(0, space);
    const rest = space < 0 ? '' : data.slice(space + 1);
    switch (verb) {
      case 'exit':
        for (const l of this.exitListeners) l(Number(rest || 0));
        break;
      case 'pong':
        if (this.pingSentAt !== 0) {
          this.latencyMs = Math.max(0, Math.round(performance.now() - this.pingSentAt));
          this.pingSentAt = 0;
        }
        break;
      case 'stats': {
        try {
          const parsed = JSON.parse(rest) as Omit<LiveStats, 'latencyMs'>;
          const stats: LiveStats = { ...parsed, latencyMs: this.latencyMs };
          for (const l of this.statsListeners) l(stats);
        } catch {
          // A malformed push is dropped; the next tick brings another.
        }
        break;
      }
      default:
        break;
    }
  }

  dispose(): void {
    if (this.pingTimer !== null) clearInterval(this.pingTimer);
    this.pingTimer = null;
    this.socket?.close(1000);
    this.socket = null;
    this.dataListeners.clear();
    this.exitListeners.clear();
    this.statsListeners.clear();
  }
}

/**
 * Pick the transport for the page: `?ws=ws://127.0.0.1:7703` (or `?ws` alone
 * for that default) attaches to a running `rusty_term_web_bridge`; without
 * the parameter the offline loopback demo runs. Returns the transport and
 * the URL to hand its `connect()`.
 */
export function transportFromLocation(search: string): { transport: TerminalTransport; url: string } {
  const ws = new URLSearchParams(search).get('ws');
  if (ws === null) {
    return { transport: new LoopbackTransport(), url: '' };
  }
  return { transport: new WebSocketTransport(), url: ws || 'ws://127.0.0.1:7703' };
}

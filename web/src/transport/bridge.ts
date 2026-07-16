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
}

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

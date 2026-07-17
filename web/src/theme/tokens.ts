/**
 * Nebula design tokens — the single source of truth for the rusty_term web
 * frontend's visual language. Tailwind's config imports from here so the
 * utility classes and any runtime styling (e.g. the xterm.js theme) stay in
 * lockstep.
 */

export const colors = {
  /** Near-black canvas with a hint of blue. */
  bg: '#0A0A0F',
  /** Elevated surfaces: cards, dock panels, the status ribbon. */
  surface: 'rgba(255, 255, 255, 0.03)',
  /** Primary foreground text. */
  text: '#E8E8F0',
  /** Primary accent — electric cyan. Focus rings, running state, the AI orb. */
  accent: '#4CE1F7',
  /** Secondary accent — warm amber. Highlights, prompt glyphs. */
  accent2: '#F7C14C',
  success: '#4CF7A2',
  warning: '#F7C14C',
  error: '#FF5F5F',
  info: '#4CE1F7',
} as const;

export type ThemeColors = { [K in keyof typeof colors]: string };
export type ThemeName = 'nebula' | 'cyberpunk' | 'minimal';

/**
 * The three presets the spec's `theme` prop promises. Nebula is the design
 * system's home look; cyberpunk trades the cyan/amber pair for hot pink on
 * a violet-black canvas; minimal desaturates everything to a quiet
 * monochrome (still dark — the hairline `white/…` borders used throughout
 * assume a dark canvas).
 */
export const themes: Record<ThemeName, ThemeColors> = {
  nebula: { ...colors },
  cyberpunk: {
    bg: '#0B0014',
    surface: 'rgba(255, 42, 109, 0.05)',
    text: '#F0E6FF',
    accent: '#FF2A6D',
    accent2: '#05FFA1',
    success: '#05FFA1',
    warning: '#FFD300',
    error: '#FF3B3B',
    info: '#00C2FF',
  },
  minimal: {
    bg: '#0E0E11',
    surface: 'rgba(255, 255, 255, 0.04)',
    text: '#DEDEE4',
    accent: '#8FA8C7',
    accent2: '#A8A8B4',
    success: '#8FC7A5',
    warning: '#C7B98F',
    error: '#C78F8F',
    info: '#8FA8C7',
  },
};

export const THEME_NAMES = Object.keys(themes) as ThemeName[];

/** '#4CE1F7' → '76 225 247', the channel form Tailwind's alpha slot needs. */
function hexTriplet(hex: string): string {
  const n = parseInt(hex.slice(1), 16);
  return `${(n >> 16) & 0xff} ${(n >> 8) & 0xff} ${n & 0xff}`;
}

/**
 * The CSS custom properties a theme resolves to. Everything except
 * `surface` (which carries its own alpha) is an RGB triplet so Tailwind
 * opacity modifiers (`text-nebula-accent/40`) keep working.
 */
export function themeCssVars(name: ThemeName): Record<string, string> {
  const t = themes[name];
  return {
    '--nebula-bg': hexTriplet(t.bg),
    '--nebula-surface': t.surface,
    '--nebula-text': hexTriplet(t.text),
    '--nebula-accent': hexTriplet(t.accent),
    '--nebula-accent2': hexTriplet(t.accent2),
    '--nebula-success': hexTriplet(t.success),
    '--nebula-warning': hexTriplet(t.warning),
    '--nebula-error': hexTriplet(t.error),
    '--nebula-info': hexTriplet(t.info),
  };
}


export const radii = {
  sm: '6px',
  md: '10px',
  lg: '16px',
} as const;

export const shadows = {
  soft: '0 4px 12px rgba(0, 0, 0, 0.35)',
} as const;

export const motion = {
  /** Standard easing for all micro-interactions. */
  easing: 'cubic-bezier(0.4, 0, 0.2, 1)',
  /** Durations in ms. Nebula stays snappy: 80–120ms. */
  duration: {
    fast: 80,
    base: 100,
    slow: 120,
  },
} as const;

export const fonts = {
  /** Commands typed by the user. */
  command: [
    'JetBrains Mono',
    'Cascadia Code',
    'ui-monospace',
    'SFMono-Regular',
    'Menlo',
    'Consolas',
    'monospace',
  ],
  /** Program output. */
  output: [
    'Cascadia Code',
    'JetBrains Mono',
    'ui-monospace',
    'SFMono-Regular',
    'Menlo',
    'Consolas',
    'monospace',
  ],
  /** Metadata: timestamps, durations, exit codes, chips. */
  meta: [
    'Inter',
    'system-ui',
    '-apple-system',
    'Segoe UI',
    'Roboto',
    'sans-serif',
  ],
} as const;

/** '#4CE1F7' + 0.25 → 'rgba(76, 225, 247, 0.25)'. */
function withAlpha(hex: string, alpha: number): string {
  const n = parseInt(hex.slice(1), 16);
  return `rgba(${(n >> 16) & 0xff}, ${(n >> 8) & 0xff}, ${n & 0xff}, ${alpha})`;
}

/**
 * ANSI 16-color palette for the raw xterm.js panel, tuned to sit inside the
 * Nebula canvas without vibrating against it.
 */
export const ansiPalette = {
  background: colors.bg,
  foreground: colors.text,
  cursor: colors.accent,
  cursorAccent: colors.bg,
  selectionBackground: 'rgba(76, 225, 247, 0.25)',
  black: '#14141C',
  red: colors.error,
  green: colors.success,
  yellow: colors.warning,
  blue: '#5FA8FF',
  magenta: '#C792EA',
  cyan: colors.accent,
  white: '#D8D8E4',
  brightBlack: '#3A3A4A',
  brightRed: '#FF8080',
  brightGreen: '#7CFFBE',
  brightYellow: '#FFD87C',
  brightBlue: '#8AC2FF',
  brightMagenta: '#DDB3F5',
  brightCyan: '#8AEEFF',
  brightWhite: '#F4F4FA',
} as const;

/**
 * The xterm.js theme for a preset: canvas, text, cursor, and selection
 * follow the theme; the 16 ANSI slots are program-chosen colors and stay
 * put so `ls --color` output looks the same everywhere.
 */
export function ansiPaletteFor(name: ThemeName): { [K in keyof typeof ansiPalette]: string } {
  const t = themes[name];
  return {
    ...ansiPalette,
    background: t.bg,
    foreground: t.text,
    cursor: t.accent,
    cursorAccent: t.bg,
    selectionBackground: withAlpha(t.accent, 0.25),
  };
}

export const tokens = { colors, radii, shadows, motion, fonts, ansiPalette } as const;
export default tokens;

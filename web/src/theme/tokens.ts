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

export const tokens = { colors, radii, shadows, motion, fonts, ansiPalette } as const;
export default tokens;

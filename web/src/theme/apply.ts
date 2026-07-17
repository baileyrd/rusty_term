/**
 * Runtime half of the theme switcher, split from tokens.ts so the token
 * module stays DOM-free (Tailwind's node-side config imports it).
 */

import { themeCssVars, type ThemeName } from './tokens';

/** Stamp a theme onto the document: custom properties + data-theme. */
export function applyTheme(name: ThemeName): void {
  const root = document.documentElement;
  for (const [k, v] of Object.entries(themeCssVars(name))) {
    root.style.setProperty(k, v);
  }
  root.dataset.theme = name;
}

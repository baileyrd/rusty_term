import type { Config } from 'tailwindcss';
import { radii, shadows, motion, fonts } from './src/theme/tokens';

export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        // Runtime-themeable: RGB-triplet custom properties set by
        // applyTheme() (src/theme/tokens.ts), nebula defaults in index.css.
        nebula: {
          bg: 'rgb(var(--nebula-bg) / <alpha-value>)',
          surface: 'var(--nebula-surface)',
          text: 'rgb(var(--nebula-text) / <alpha-value>)',
          accent: 'rgb(var(--nebula-accent) / <alpha-value>)',
          accent2: 'rgb(var(--nebula-accent2) / <alpha-value>)',
          success: 'rgb(var(--nebula-success) / <alpha-value>)',
          warning: 'rgb(var(--nebula-warning) / <alpha-value>)',
          error: 'rgb(var(--nebula-error) / <alpha-value>)',
          info: 'rgb(var(--nebula-info) / <alpha-value>)',
        },
      },
      borderRadius: {
        'nebula-sm': radii.sm,
        'nebula-md': radii.md,
        'nebula-lg': radii.lg,
      },
      boxShadow: {
        'nebula-soft': shadows.soft,
      },
      fontFamily: {
        'nebula-command': [...fonts.command],
        'nebula-output': [...fonts.output],
        'nebula-meta': [...fonts.meta],
      },
      transitionTimingFunction: {
        nebula: motion.easing,
      },
      transitionDuration: {
        'nebula-fast': `${motion.duration.fast}ms`,
        'nebula-base': `${motion.duration.base}ms`,
        'nebula-slow': `${motion.duration.slow}ms`,
      },
      keyframes: {
        'nebula-fade-in': {
          from: { opacity: '0', transform: 'translateY(2px)' },
          to: { opacity: '1', transform: 'translateY(0)' },
        },
        'nebula-pulse': {
          '0%, 100%': { boxShadow: '0 0 0 0 rgb(var(--nebula-accent) / 0.45)' },
          '50%': { boxShadow: '0 0 0 10px rgb(var(--nebula-accent) / 0)' },
        },
      },
      animation: {
        'nebula-fade-in': `nebula-fade-in ${motion.duration.fast}ms ${motion.easing} both`,
        'nebula-pulse': `nebula-pulse 2.4s ${motion.easing} infinite`,
      },
    },
  },
  plugins: [],
} satisfies Config;

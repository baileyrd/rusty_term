import type { Config } from 'tailwindcss';
import { colors, radii, shadows, motion, fonts } from './src/theme/tokens';

export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        nebula: {
          bg: colors.bg,
          surface: colors.surface,
          text: colors.text,
          accent: colors.accent,
          accent2: colors.accent2,
          success: colors.success,
          warning: colors.warning,
          error: colors.error,
          info: colors.info,
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
          '0%, 100%': { boxShadow: '0 0 0 0 rgba(76, 225, 247, 0.45)' },
          '50%': { boxShadow: '0 0 0 10px rgba(76, 225, 247, 0)' },
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

import React from 'react';
import { theme } from '../theme';

// The Areev mark: two overlapping rounded squares (from the console wordmark).
export const Logo: React.FC<{ size?: number }> = ({ size = 96 }) => (
  <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden>
    <rect
      x="7.5"
      y="2.5"
      width="14"
      height="14"
      rx="4.5"
      fill="none"
      stroke={theme.teal}
      strokeWidth={2}
    />
    <rect x="2.5" y="7.5" width="14" height="14" rx="4.5" fill={theme.accent} />
  </svg>
);

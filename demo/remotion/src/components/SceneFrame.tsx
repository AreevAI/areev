import React from 'react';
import { AbsoluteFill, useCurrentFrame, interpolate } from 'remotion';
import { theme } from '../theme';
import { Logo } from './Logo';

// Scene-level cross-fades are handled by the TransitionSeries in AreevDemo, so
// this only owns the content's intro motion, not the fade in/out.
export const SceneFrame: React.FC<{
  label: string;
  caption: React.ReactNode;
  children: React.ReactNode;
}> = ({ label, caption, children }) => {
  const frame = useCurrentFrame();
  const rise = interpolate(frame, [0, 14], [26, 0], { extrapolateRight: 'clamp' });

  return (
    <AbsoluteFill style={{ backgroundColor: theme.bg }}>
      {/* brand chip top-left */}
      <div style={{ position: 'absolute', top: 54, left: 64, display: 'flex', alignItems: 'center', gap: 14 }}>
        <Logo size={40} />
        <span style={{ fontFamily: theme.sans, fontSize: 34, fontWeight: 800, color: theme.bright, letterSpacing: -1 }}>
          areev
        </span>
      </div>

      {/* beat label pill top-right */}
      <div
        style={{
          position: 'absolute',
          top: 60,
          right: 64,
          padding: '8px 20px',
          borderRadius: 999,
          border: `1px solid ${theme.accentSoft}`,
          backgroundColor: theme.accentSoft,
          color: theme.accent,
          fontFamily: theme.mono,
          fontSize: 24,
          letterSpacing: 1,
        }}
      >
        {label}
      </div>

      {/* center content — padded to stay clear of the header and the caption */}
      <AbsoluteFill style={{ justifyContent: 'center', alignItems: 'center', padding: '150px 80px 240px' }}>
        <div style={{ transform: `translateY(${rise}px)` }}>{children}</div>
      </AbsoluteFill>

      {/* lower-third caption */}
      <div
        style={{
          position: 'absolute',
          bottom: 70,
          left: 0,
          right: 0,
          textAlign: 'center',
          padding: '0 200px',
          fontFamily: theme.sans,
          fontSize: 38,
          lineHeight: 1.4,
          color: theme.dim,
        }}
      >
        {caption}
      </div>
    </AbsoluteFill>
  );
};

// caption text highlight
export const Hi: React.FC<{ children: React.ReactNode; c?: string }> = ({ children, c }) => (
  <span style={{ color: c ?? theme.bright, fontWeight: 600 }}>{children}</span>
);

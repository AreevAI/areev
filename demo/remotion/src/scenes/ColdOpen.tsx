import React from 'react';
import {
  AbsoluteFill,
  useCurrentFrame,
  useVideoConfig,
  interpolate,
  spring,
} from 'remotion';
import { theme } from '../theme';
import { Logo } from '../components/Logo';

export const ColdOpen: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();

  const logoIn = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 30 });
  const logoScale = interpolate(logoIn, [0, 1], [0.6, 1]);
  const wordIn = interpolate(frame, [14, 30], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });
  const wordX = interpolate(wordIn, [0, 1], [24, 0]);
  const taglineIn = interpolate(frame, [34, 52], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });
  const taglineY = interpolate(taglineIn, [0, 1], [18, 0]);

  return (
    <AbsoluteFill style={{ backgroundColor: theme.bg }}>
      <AbsoluteFill style={{ justifyContent: 'center', alignItems: 'center' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 28 }}>
          <div style={{ transform: `scale(${logoScale})`, opacity: logoIn }}>
            <Logo size={128} />
          </div>
          <div style={{ opacity: wordIn, transform: `translateX(${wordX}px)`, display: 'flex', alignItems: 'baseline', gap: 18 }}>
            <span style={{ fontFamily: theme.sans, fontSize: 120, fontWeight: 800, color: theme.bright, letterSpacing: -3 }}>
              areev
            </span>
            <span style={{ fontFamily: theme.mono, fontSize: 40, color: theme.teal, letterSpacing: 2 }}>1.0</span>
          </div>
        </div>
        <div
          style={{
            marginTop: 44,
            opacity: taglineIn,
            transform: `translateY(${taglineY}px)`,
            fontFamily: theme.sans,
            fontSize: 40,
            color: theme.dim,
            fontWeight: 400,
            textAlign: 'center',
            maxWidth: 1200,
            lineHeight: 1.4,
          }}
        >
          Your agent's memory should be{' '}
          <span style={{ color: theme.text, fontWeight: 600 }}>a file you own</span> — one that{' '}
          <span style={{ color: theme.text, fontWeight: 600 }}>can't quietly rot.</span>
        </div>
      </AbsoluteFill>
    </AbsoluteFill>
  );
};

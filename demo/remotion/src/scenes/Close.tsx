import React from 'react';
import { AbsoluteFill, useCurrentFrame, useVideoConfig, interpolate, spring } from 'remotion';
import { theme } from '../theme';
import { Logo } from '../components/Logo';
import { Counter } from '../components/Counter';

const Stat: React.FC<{ children: React.ReactNode; small: string; delay: number; color: string }> = ({ children, small, delay, color }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const s = spring({ frame: frame - delay, fps, config: { damping: 200 }, durationInFrames: 20 });
  return (
    <div style={{ opacity: s, transform: `translateY(${interpolate(s, [0, 1], [20, 0])}px)`, textAlign: 'center', minWidth: 360 }}>
      <div style={{ fontFamily: theme.mono, fontSize: 62, fontWeight: 700, color }}>{children}</div>
      <div style={{ fontFamily: theme.sans, fontSize: 28, color: theme.dim, marginTop: 8 }}>{small}</div>
    </div>
  );
};

export const Close: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const logoIn = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 24 });
  const tagIn = interpolate(frame, [16, 32], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });
  const urlIn = interpolate(frame, [64, 82], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });

  return (
    <AbsoluteFill style={{ backgroundColor: theme.bg, justifyContent: 'center', alignItems: 'center' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 22, opacity: logoIn, transform: `scale(${interpolate(logoIn, [0, 1], [0.8, 1])})` }}>
        <Logo size={78} />
        <span style={{ fontFamily: theme.sans, fontSize: 84, fontWeight: 800, color: theme.bright, letterSpacing: -2 }}>areev</span>
        <span style={{ fontFamily: theme.mono, fontSize: 34, color: theme.teal }}>1.0</span>
      </div>

      <div style={{ marginTop: 26, opacity: tagIn, fontFamily: theme.sans, fontSize: 38, color: theme.dim, textAlign: 'center' }}>
        The embedded memory engine for AI agents — <span style={{ color: theme.text, fontWeight: 600 }}>memory that doesn't rot.</span>
      </div>

      <div style={{ marginTop: 64, display: 'flex', gap: 90, alignItems: 'flex-start' }}>
        <Stat small="structural recall, in-process" delay={34} color={theme.teal}>
          <Counter to={28} delay={40} dur={34} prefix="~" suffix="µs" />
        </Stat>
        <Stat small="LoCoMo hit@20 (retrieval)" delay={44} color={theme.accent}>
          <Counter to={81.6} delay={50} dur={38} decimals={1} suffix="%" />
        </Stat>
        <Stat small="dedup · staleness · provenance, measured" delay={54} color={theme.sky}>
          0 LLM
        </Stat>
      </div>

      <div style={{ marginTop: 72, opacity: urlIn, textAlign: 'center' }}>
        <div style={{ fontFamily: theme.mono, fontSize: 42, color: theme.bright }}>github.com/AreevAI/areev</div>
        <div style={{ fontFamily: theme.sans, fontSize: 26, color: theme.dimmer, marginTop: 12 }}>open source · MIT OR Apache-2.0</div>
      </div>
    </AbsoluteFill>
  );
};

import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';
import { SceneFrame, Hi } from '../components/SceneFrame';
import { MemoryCard } from '../components/MemoryCard';
import { Counter } from '../components/Counter';
import { theme } from '../theme';

const N = 7;

// The problem, shown not told: one fact duplicates into a messy pile while a
// counter races up, then the top copy goes stale.
export const RotScene: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const staleP = interpolate(frame, [78, 96], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });
  const glow = interpolate(frame, [24, 70], [0, 0.45], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });

  return (
    <SceneFrame
      label="memory rots"
      caption={
        <>
          The same fact, <Hi c={theme.red}>hundreds of times</Hi> — and no idea why.
        </>
      }
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 90 }}>
        {/* duplicate pile */}
        <div style={{ position: 'relative', width: 560, height: 460, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
          <div
            style={{
              position: 'absolute',
              width: 460,
              height: 300,
              borderRadius: '50%',
              background: `radial-gradient(closest-side, ${theme.red}, transparent)`,
              filter: 'blur(70px)',
              opacity: glow,
            }}
          />
          {Array.from({ length: N }).map((_, i) => {
            const delay = 8 + i * 5;
            const p = spring({ frame: frame - delay, fps, config: { damping: 200 }, durationInFrames: 16 });
            const dx = (i - 3) * 22;
            const dy = -(i - 3) * 8;
            const rot = (i - 3) * 3.6;
            const top = i === N - 1;
            return (
              <div
                key={i}
                style={{
                  position: 'absolute',
                  opacity: p * (top ? 1 : 0.55 + 0.4 * (i / N)),
                  transform: `translate(${dx * p}px, ${dy * p}px) rotate(${rot * p}deg) scale(${interpolate(p, [0, 1], [0.7, 1])})`,
                  zIndex: i,
                }}
              >
                <MemoryCard
                  subject="john"
                  relation="prefers"
                  object='"window seat"'
                  width={380}
                  status={top && staleP > 0.5 ? 'stale' : 'plain'}
                />
              </div>
            );
          })}
        </div>

        {/* runaway counter */}
        <div style={{ textAlign: 'center' }}>
          <div style={{ fontFamily: theme.mono, fontSize: 150, fontWeight: 800, color: theme.red, lineHeight: 1 }}>
            <Counter from={1} to={247} delay={12} dur={64} prefix="×" />
          </div>
          <div style={{ fontFamily: theme.sans, fontSize: 34, color: theme.dimmer, marginTop: 10 }}>copies of one grain</div>
        </div>
      </div>
    </SceneFrame>
  );
};

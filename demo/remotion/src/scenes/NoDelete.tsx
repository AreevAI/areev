import React from 'react';
import { useCurrentFrame, interpolate, spring, useVideoConfig } from 'remotion';
import { SceneFrame, Hi } from '../components/SceneFrame';
import { theme } from '../theme';

const Pill: React.FC<{ children: React.ReactNode; color: string; bg: string; line: string; strike?: boolean; delay: number }>
  = ({ children, color, bg, line, strike, delay }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const s = spring({ frame: frame - delay, fps, config: { damping: 200 }, durationInFrames: 18 });
  return (
    <div
      style={{
        opacity: s,
        transform: `scale(${interpolate(s, [0, 1], [0.8, 1])})`,
        padding: '14px 30px',
        borderRadius: 14,
        border: `1px solid ${line}`,
        backgroundColor: bg,
        color,
        fontFamily: theme.mono,
        fontSize: 40,
        fontWeight: 600,
        textDecoration: strike ? 'line-through' : 'none',
        textDecorationColor: theme.red,
        textDecorationThickness: 3,
      }}
    >
      {children}
    </div>
  );
};

export const NoDelete: React.FC = () => (
  <SceneFrame
    label="gated by design"
    caption={
      <>
        <Hi>No bulk delete</Hi> — <Hi c={theme.red}>DELETE</Hi> / <Hi c={theme.red}>DROP</Hi> aren't even tokens.
      </>
    }
  >
    <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 34, width: 1300 }}>
      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 20, justifyContent: 'center' }}>
        {['RECALL', 'ASSEMBLE', 'HISTORY', 'ADD', 'SUPERSEDE'].map((v, i) => (
          <Pill key={v} color={theme.teal} bg={theme.well} line={theme.line2} delay={6 + i * 5}>
            {v}
          </Pill>
        ))}
      </div>
      <div style={{ display: 'flex', gap: 24, alignItems: 'center' }}>
        <Pill color={theme.red} bg={theme.errBg} line={'#5A2B2E'} strike delay={40}>
          DELETE
        </Pill>
        <Pill color={theme.red} bg={theme.errBg} line={'#5A2B2E'} strike delay={48}>
          DROP
        </Pill>
        <span style={{ fontFamily: theme.sans, fontSize: 30, color: theme.dimmer }}>not tokens in the grammar</span>
      </div>
      <div style={{ display: 'flex', gap: 22, alignItems: 'center' }}>
        <Pill color={theme.amber} bg={theme.well} line={'#4E4020'} delay={62}>
          🔒 FORGET &lt;hash&gt;
        </Pill>
        <span style={{ fontFamily: theme.sans, fontSize: 30, color: theme.dimmer }}>gated · single grain · one tombstone</span>
      </div>
    </div>
  </SceneFrame>
);

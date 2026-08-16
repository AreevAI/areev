import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';
import { theme } from '../theme';

export type CheckItem = { text: string; ok: boolean; sub?: string };

const Mark: React.FC<{ ok: boolean; p: number }> = ({ ok, p }) => (
  <div
    style={{
      width: 58,
      height: 58,
      flexShrink: 0,
      borderRadius: 14,
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'center',
      backgroundColor: ok ? theme.okBg : theme.errBg,
      border: `1px solid ${ok ? '#2E4A33' : '#5A2B2E'}`,
      transform: `scale(${interpolate(p, [0, 1], [0.6, 1])})`,
    }}
  >
    <span style={{ fontSize: 34, color: ok ? theme.green : theme.red, fontWeight: 800 }}>{ok ? '✓' : '✕'}</span>
  </div>
);

export const Checklist: React.FC<{ items: CheckItem[]; startAt?: number; stagger?: number; width?: number }> = ({
  items,
  startAt = 6,
  stagger = 12,
  width = 1180,
}) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 22, width }}>
      {items.map((it, i) => {
        const delay = startAt + i * stagger;
        const p = spring({ frame: frame - delay, fps, config: { damping: 200 }, durationInFrames: 16 });
        return (
          <div
            key={i}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 26,
              opacity: p,
              transform: `translateX(${interpolate(p, [0, 1], [-30, 0])}px)`,
              backgroundColor: theme.panel,
              border: `1px solid ${theme.line}`,
              borderRadius: 16,
              padding: '20px 28px',
            }}
          >
            <Mark ok={it.ok} p={p} />
            <div>
              <div style={{ fontFamily: theme.sans, fontSize: 40, color: theme.bright, fontWeight: 600 }}>{it.text}</div>
              {it.sub && <div style={{ fontFamily: theme.mono, fontSize: 26, color: theme.dimmer, marginTop: 4 }}>{it.sub}</div>}
            </div>
          </div>
        );
      })}
    </div>
  );
};

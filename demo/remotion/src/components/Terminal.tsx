import React from 'react';
import { useCurrentFrame, interpolate } from 'remotion';
import { theme } from '../theme';

export type OutLine = { t: string; c?: string; b?: boolean };
export type Block = { cmd: string; out?: OutLine[]; hold?: number };

// A fake terminal that types each command char-by-char, then reveals its
// output, then moves to the next block. Timing is a pure function of the frame
// so it renders deterministically. `cps` ≈ characters typed per frame.
export const Terminal: React.FC<{
  title?: string;
  blocks: Block[];
  startAt?: number;
  cps?: number;
  outGap?: number;
  width?: number;
}> = ({ title = 'areev — caller.db', blocks, startAt = 0, cps = 1.6, outGap = 8, width = 1480 }) => {
  const frame = useCurrentFrame();

  // Walk the blocks accumulating frame offsets.
  let cursor = startAt;
  const timed = blocks.map((b) => {
    const typeStart = cursor;
    const typeFrames = Math.max(6, Math.round(b.cmd.length / cps));
    const typeEnd = typeStart + typeFrames;
    const outStart = typeEnd + outGap;
    const outFrames = (b.out?.length ?? 0) * 3 + 6;
    cursor = outStart + outFrames + (b.hold ?? 14);
    return { ...b, typeStart, typeEnd, outStart };
  });

  return (
    <div
      style={{
        width,
        borderRadius: 16,
        overflow: 'hidden',
        border: `1px solid ${theme.line2}`,
        backgroundColor: theme.panel,
        boxShadow: '0 40px 120px rgba(0,0,0,0.55)',
        fontFamily: theme.mono,
      }}
    >
      {/* title bar */}
      <div
        style={{
          height: 52,
          display: 'flex',
          alignItems: 'center',
          gap: 10,
          padding: '0 20px',
          backgroundColor: theme.well,
          borderBottom: `1px solid ${theme.line}`,
        }}
      >
        <Dot c={theme.red} />
        <Dot c={theme.amber} />
        <Dot c={theme.green} />
        <div style={{ flex: 1, textAlign: 'center', color: theme.dimmer, fontSize: 22, letterSpacing: 0.5 }}>{title}</div>
        <div style={{ width: 54 }} />
      </div>

      {/* body */}
      <div style={{ padding: '28px 34px', fontSize: 26, lineHeight: 1.65, minHeight: 110 }}>
        {timed.map((b, i) => {
          if (frame < b.typeStart) return null;
          const chars = Math.floor(interpolate(frame, [b.typeStart, b.typeEnd], [0, b.cmd.length], {
            extrapolateLeft: 'clamp',
            extrapolateRight: 'clamp',
          }));
          const typing = frame >= b.typeStart && frame < b.typeEnd;
          const cursorOn = Math.floor(frame / 8) % 2 === 0;
          return (
            <div key={i} style={{ marginBottom: 6 }}>
              <div style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}>
                <span style={{ color: theme.teal, fontWeight: 700 }}>❯ </span>
                <span style={{ color: theme.bright }}>{b.cmd.slice(0, chars)}</span>
                {typing && cursorOn && (
                  <span style={{ color: theme.accent }}>▋</span>
                )}
              </div>
              {b.out?.map((o, j) => {
                const at = b.outStart + j * 3;
                const op = interpolate(frame, [at, at + 6], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });
                if (frame < at) return null;
                return (
                  <div
                    key={j}
                    style={{
                      opacity: op,
                      color: o.c ?? theme.dim,
                      fontWeight: o.b ? 700 : 400,
                      whiteSpace: 'pre-wrap',
                      wordBreak: 'break-word',
                    }}
                  >
                    {o.t}
                  </div>
                );
              })}
            </div>
          );
        })}
      </div>
    </div>
  );
};

const Dot: React.FC<{ c: string }> = ({ c }) => (
  <div style={{ width: 14, height: 14, borderRadius: 7, backgroundColor: c, opacity: 0.85 }} />
);

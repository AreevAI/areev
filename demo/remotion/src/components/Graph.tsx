import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';
import { theme } from '../theme';

const W = 1180;
const H = 500;

type Node = { id: string; x: number; y: number; label: string; kind: 'subject' | 'current' | 'stale' };
type Edge = { from: string; to: string; label: string; stale?: boolean };

const NODES: Node[] = [
  { id: 'john', x: 250, y: 250, label: 'john', kind: 'subject' },
  { id: 'aisle', x: 770, y: 110, label: '"aisle seat"', kind: 'current' },
  { id: 'window', x: 830, y: 270, label: '"window seat"', kind: 'stale' },
  { id: 'veg', x: 640, y: 430, label: '"vegetarian"', kind: 'current' },
];
const EDGES: Edge[] = [
  { from: 'john', to: 'aisle', label: 'prefers' },
  { from: 'john', to: 'window', label: 'prefers', stale: true },
  { from: 'john', to: 'veg', label: 'diet' },
];
const byId = (id: string) => NODES.find((n) => n.id === id)!;

export const Graph: React.FC<{ startAt?: number }> = ({ startAt = 0 }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const f = frame - startAt;

  return (
    <div style={{ position: 'relative', width: W, height: H }}>
      <svg width={W} height={H} style={{ position: 'absolute', inset: 0 }}>
        {EDGES.map((e, i) => {
          const a = byId(e.from);
          const b = byId(e.to);
          const grow = interpolate(f, [10 + i * 6, 34 + i * 6], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });
          const mx = a.x + (b.x - a.x) * grow;
          const my = a.y + (b.y - a.y) * grow;
          const lx = (a.x + b.x) / 2;
          const ly = (a.y + b.y) / 2;
          const col = e.stale ? theme.line2 : theme.teal;
          return (
            <g key={i}>
              <line
                x1={a.x}
                y1={a.y}
                x2={mx}
                y2={my}
                stroke={col}
                strokeWidth={e.stale ? 3 : 4}
                strokeDasharray={e.stale ? '10 8' : undefined}
              />
              <g opacity={interpolate(f, [30 + i * 6, 42 + i * 6], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' })}>
                <rect x={lx - e.label.length * 8 - 10} y={ly - 20} width={e.label.length * 16 + 20} height={38} rx={9} fill={theme.bg} stroke={theme.line} />
                <text x={lx} y={ly + 6} textAnchor="middle" fill={e.stale ? theme.dimmer : theme.teal} fontSize={24} fontFamily={theme.mono}>
                  {e.label}
                </text>
              </g>
            </g>
          );
        })}
      </svg>

      {NODES.map((n, i) => {
        const delay = n.kind === 'subject' ? 0 : 34 + i * 6;
        const p = spring({ frame: f - delay, fps, config: { damping: 200 }, durationInFrames: 16 });
        const subject = n.kind === 'subject';
        const stale = n.kind === 'stale';
        return (
          <div
            key={n.id}
            style={{
              position: 'absolute',
              left: n.x,
              top: n.y,
              transform: `translate(-50%,-50%) scale(${interpolate(p, [0, 1], [0.5, 1])})`,
              opacity: p,
              padding: subject ? '22px 40px' : '16px 30px',
              borderRadius: 999,
              backgroundColor: subject ? theme.accent : theme.panel,
              border: `2px solid ${subject ? theme.accent : stale ? theme.line2 : theme.teal}`,
              color: subject ? '#0E1015' : stale ? theme.dimmer : theme.text,
              fontFamily: subject ? theme.sans : theme.mono,
              fontWeight: subject ? 800 : 600,
              fontSize: subject ? 44 : 30,
              whiteSpace: 'nowrap',
            }}
          >
            {n.label}
            {stale && (
              <span style={{ marginLeft: 12, fontSize: 20, color: theme.dimmer, fontFamily: theme.sans }}>· superseded</span>
            )}
          </div>
        );
      })}
    </div>
  );
};

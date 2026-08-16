import React from 'react';
import { theme } from '../theme';

type Row = { s: string; r: string; o: string; conf: string; tag?: 'current' | 'superseded' };

const ROWS: Row[] = [
  { s: 'john', r: 'prefers', o: '"aisle seat"', conf: '0.90', tag: 'current' },
  { s: 'john', r: 'prefers', o: '"window seat"', conf: '0.90', tag: 'superseded' },
  { s: 'john', r: 'diet', o: '"vegetarian"', conf: '0.95' },
  { s: 'john', r: 'allergic_to', o: '"peanuts"', conf: '0.98' },
  { s: 'fix_flaky', r: 'lesson', o: '"Isolate the shared tempdir per test."', conf: '0.70' },
];

export const MemoriesView: React.FC = () => (
  <div style={{ position: 'absolute', inset: 0, padding: '26px 30px', overflow: 'hidden' }}>
    {/* column header */}
    <div style={{ display: 'flex', gap: 20, padding: '0 20px 14px', fontFamily: theme.sans, fontSize: 22, color: theme.dimmer, borderBottom: `1px solid ${theme.line}` }}>
      <div style={{ width: 220 }}>subject</div>
      <div style={{ width: 220 }}>relation</div>
      <div style={{ flex: 1 }}>object</div>
      <div style={{ width: 130, textAlign: 'right' }}>confidence</div>
    </div>
    {ROWS.map((row, i) => {
      const stale = row.tag === 'superseded';
      return (
        <div
          key={i}
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 20,
            padding: '20px 20px',
            borderBottom: `1px solid ${theme.line}`,
            opacity: stale ? 0.5 : 1,
            fontFamily: theme.mono,
            fontSize: 27,
          }}
        >
          <div style={{ width: 220, color: theme.accent }}>{row.s}</div>
          <div style={{ width: 220, color: theme.dim }}>{row.r}</div>
          <div style={{ flex: 1, color: theme.text, display: 'flex', alignItems: 'center', gap: 14 }}>
            <span style={{ textDecoration: stale ? 'line-through' : 'none', textDecorationColor: theme.dimmer }}>{row.o}</span>
            {row.tag && (
              <span
                style={{
                  fontFamily: theme.sans,
                  fontSize: 20,
                  padding: '3px 12px',
                  borderRadius: 8,
                  color: stale ? theme.dimmer : theme.green,
                  backgroundColor: stale ? theme.raise : theme.okBg,
                  border: `1px solid ${stale ? theme.line2 : '#2E4A33'}`,
                }}
              >
                {row.tag}
              </span>
            )}
          </div>
          <div style={{ width: 130, textAlign: 'right', color: theme.teal }}>{row.conf}</div>
        </div>
      );
    })}
  </div>
);

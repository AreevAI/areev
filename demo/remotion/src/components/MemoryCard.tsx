import React from 'react';
import { theme } from '../theme';

// A memory grain rendered as a card — the core visual unit of the demo.
export const MemoryCard: React.FC<{
  subject: string;
  relation: string;
  object: string;
  conf?: string;
  status?: 'current' | 'stale' | 'plain';
  width?: number;
  style?: React.CSSProperties;
}> = ({ subject, relation, object, conf = '0.90', status = 'plain', width = 460, style }) => {
  const stale = status === 'stale';
  const border = stale ? theme.line2 : status === 'current' ? '#2E4A33' : theme.line2;
  return (
    <div
      style={{
        width,
        padding: '24px 28px',
        borderRadius: 18,
        backgroundColor: theme.panel,
        border: `1.5px solid ${border}`,
        boxShadow: '0 24px 60px rgba(0,0,0,0.45)',
        fontFamily: theme.mono,
        opacity: stale ? 0.6 : 1,
        ...style,
      }}
    >
      <div style={{ fontSize: 24, color: theme.dim }}>
        <span style={{ color: theme.accent }}>{subject}</span> · {relation}
      </div>
      <div
        style={{
          fontSize: 36,
          fontWeight: 700,
          color: theme.bright,
          margin: '8px 0 16px',
          textDecoration: stale ? 'line-through' : 'none',
          textDecorationColor: theme.dimmer,
        }}
      >
        {object}
      </div>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <span style={{ fontSize: 24, color: theme.teal }}>{conf}</span>
        {status !== 'plain' && (
          <span
            style={{
              fontFamily: theme.sans,
              fontSize: 20,
              padding: '4px 14px',
              borderRadius: 999,
              color: stale ? theme.dimmer : theme.green,
              backgroundColor: stale ? theme.raise : theme.okBg,
              border: `1px solid ${stale ? theme.line2 : '#2E4A33'}`,
            }}
          >
            {stale ? 'superseded' : 'current'}
          </span>
        )}
      </div>
    </div>
  );
};

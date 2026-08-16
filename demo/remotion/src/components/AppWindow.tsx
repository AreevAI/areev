import React from 'react';
import { theme } from '../theme';
import { Logo } from './Logo';

// A console-style app window (title bar + tab strip), matching the Areev web
// console. Recreated from the shipped console.html design (Paper was
// unavailable), so it is on-brand with the real product UI.
export const AppWindow: React.FC<{
  tabs: string[];
  active: number;
  width: number;
  height: number;
  children: React.ReactNode;
}> = ({ tabs, active, width, height, children }) => (
  <div
    style={{
      width,
      height,
      borderRadius: 18,
      overflow: 'hidden',
      border: `1px solid ${theme.line2}`,
      backgroundColor: theme.panel,
      boxShadow: '0 44px 130px rgba(0,0,0,0.55)',
      display: 'flex',
      flexDirection: 'column',
    }}
  >
    {/* title bar */}
    <div style={{ height: 58, display: 'flex', alignItems: 'center', gap: 12, padding: '0 22px', backgroundColor: theme.well, borderBottom: `1px solid ${theme.line}` }}>
      <div style={{ width: 13, height: 13, borderRadius: 7, backgroundColor: theme.red, opacity: 0.85 }} />
      <div style={{ width: 13, height: 13, borderRadius: 7, backgroundColor: theme.amber, opacity: 0.85 }} />
      <div style={{ width: 13, height: 13, borderRadius: 7, backgroundColor: theme.green, opacity: 0.85 }} />
      <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 10 }}>
        <Logo size={24} />
        <span style={{ fontFamily: theme.sans, fontWeight: 800, fontSize: 24, color: theme.bright }}>areev</span>
        <span style={{ fontFamily: theme.sans, fontSize: 22, color: theme.dimmer }}>console</span>
      </div>
      <span style={{ fontFamily: theme.mono, fontSize: 20, color: theme.dimmer }}>caller.db</span>
    </div>
    {/* tab strip */}
    <div style={{ display: 'flex', gap: 8, padding: '14px 22px 0', backgroundColor: theme.panel, borderBottom: `1px solid ${theme.line}` }}>
      {tabs.map((t, i) => (
        <div
          key={t}
          style={{
            padding: '12px 26px',
            fontFamily: theme.sans,
            fontSize: 26,
            color: i === active ? theme.bright : theme.dimmer,
            fontWeight: i === active ? 700 : 400,
            borderBottom: `3px solid ${i === active ? theme.accent : 'transparent'}`,
            marginBottom: -1,
          }}
        >
          {t}
        </div>
      ))}
    </div>
    {/* body */}
    <div style={{ flex: 1, position: 'relative', backgroundColor: theme.well }}>{children}</div>
  </div>
);

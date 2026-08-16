import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';
import { SceneFrame, Hi } from '../components/SceneFrame';
import { theme } from '../theme';

const Card: React.FC<{ tag: string; tagColor: string; title: string; sub: string; delay: number; width: number }> = ({ tag, tagColor, title, sub, delay, width }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const p = spring({ frame: frame - delay, fps, config: { damping: 200 }, durationInFrames: 18 });
  return (
    <div
      style={{
        width,
        opacity: p,
        transform: `translateY(${interpolate(p, [0, 1], [24, 0])}px)`,
        backgroundColor: theme.panel,
        border: `1px solid ${theme.line2}`,
        borderRadius: 18,
        padding: '26px 30px',
      }}
    >
      <div style={{ fontFamily: theme.mono, fontSize: 22, letterSpacing: 2, color: tagColor, marginBottom: 12 }}>{tag}</div>
      <div style={{ fontFamily: theme.sans, fontSize: 34, color: theme.bright, fontWeight: 600, lineHeight: 1.3 }}>{title}</div>
      <div style={{ fontFamily: theme.mono, fontSize: 22, color: theme.dimmer, marginTop: 12 }}>{sub}</div>
    </div>
  );
};

const Arrow: React.FC<{ delay: number; label: string }> = ({ delay, label }) => {
  const frame = useCurrentFrame();
  const o = interpolate(frame, [delay, delay + 12], [0, 1], { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' });
  return (
    <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 8, opacity: o }}>
      <div style={{ fontFamily: theme.mono, fontSize: 22, color: theme.teal }}>{label}</div>
      <div style={{ fontSize: 52, color: theme.teal, lineHeight: 1 }}>→</div>
    </div>
  );
};

// Beat — Safe for agents that learn (provenance chain)
export const Provenance: React.FC = () => (
  <SceneFrame
    label="safe to learn"
    caption={
      <>
        Every lesson traces to why — <Hi>undo a bad session</Hi>.
      </>
    }
  >
    <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 44 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 40 }}>
        <Card
          tag="EXPERIENCE"
          tagColor={theme.sky}
          title="“session 41: fixed the flaky test by isolating the tempdir”"
          sub="observation · 80f2942b…"
          delay={6}
          width={620}
        />
        <Arrow delay={30} label="derived_from" />
        <Card
          tag="LESSON"
          tagColor={theme.accent}
          title="“Isolate the shared tempdir per test.”"
          sub="fix_flaky · lesson"
          delay={44}
          width={560}
        />
      </div>
      <div style={{ display: 'flex', gap: 20 }}>
        {[
          ['areev provenance', 'trace it'],
          ['supersede', 'revise it'],
          ['restore --until-hlc', 'undo the session'],
        ].map(([cmd, what], i) => (
          <Pill key={i} cmd={cmd} what={what} delay={64 + i * 8} />
        ))}
      </div>
    </div>
  </SceneFrame>
);

const Pill: React.FC<{ cmd: string; what: string; delay: number }> = ({ cmd, what, delay }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const p = spring({ frame: frame - delay, fps, config: { damping: 200 }, durationInFrames: 14 });
  return (
    <div
      style={{
        opacity: p,
        transform: `scale(${interpolate(p, [0, 1], [0.85, 1])})`,
        display: 'flex',
        alignItems: 'center',
        gap: 12,
        padding: '12px 22px',
        borderRadius: 12,
        backgroundColor: theme.well,
        border: `1px solid ${theme.line2}`,
      }}
    >
      <span style={{ fontFamily: theme.mono, fontSize: 24, color: theme.teal }}>{cmd}</span>
      <span style={{ fontFamily: theme.sans, fontSize: 24, color: theme.dim }}>→ {what}</span>
    </div>
  );
};

// Beat — one line for an agent
export const OneLineCard: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const cmdIn = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 20 });
  const okIn = spring({ frame: frame - 26, fps, config: { damping: 200 }, durationInFrames: 18 });
  return (
    <SceneFrame
      label="model-native"
      caption={
        <>
          One line. Any MCP client. <Hi>It's just MCP.</Hi>
        </>
      }
    >
      <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 40, width: 1400 }}>
        <div
          style={{
            opacity: cmdIn,
            transform: `translateY(${interpolate(cmdIn, [0, 1], [20, 0])}px)`,
            width: '100%',
            backgroundColor: theme.panel,
            border: `1px solid ${theme.line2}`,
            borderRadius: 16,
            padding: '34px 40px',
            fontFamily: theme.mono,
            fontSize: 38,
            color: theme.bright,
            textAlign: 'center',
          }}
        >
          <span style={{ color: theme.dimmer }}>$ </span>
          claude mcp add areev <span style={{ color: theme.teal }}>--</span> areev serve --mcp
        </div>
        <div
          style={{
            opacity: okIn,
            transform: `scale(${interpolate(okIn, [0, 1], [0.9, 1])})`,
            display: 'flex',
            alignItems: 'center',
            gap: 16,
            fontFamily: theme.sans,
            fontSize: 34,
            color: theme.green,
          }}
        >
          <span style={{ fontSize: 40 }}>✓</span> persistent memory across sessions — it's just MCP
        </div>
      </div>
    </SceneFrame>
  );
};

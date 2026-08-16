import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';
import { SceneFrame, Hi } from '../components/SceneFrame';
import { MemoryCard } from '../components/MemoryCard';
import { theme } from '../theme';

const N = 7;
const clamp = { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' } as const;

// The fix, shown: the duplicate pile collapses into one grain, then the value
// supersedes — old slides down to history, new slides into place.
export const DedupScene: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const collapse = interpolate(frame, [26, 54], [0, 1], clamp);
  const survivorIn = interpolate(frame, [44, 60], [0, 1], clamp);
  const badge = interpolate(frame, [56, 70], [0, 1], clamp);
  const sup = interpolate(frame, [80, 110], [0, 1], clamp);
  const newIn = interpolate(frame, [82, 100], [0, 1], clamp);

  return (
    <SceneFrame
      label="can't rot"
      caption={
        <>
          Collapse to <Hi c={theme.green}>one grain</Hi> · edits supersede · history kept.
        </>
      }
    >
      <div style={{ position: 'relative', width: 900, height: 560, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        {/* the duplicates flying into one */}
        {Array.from({ length: N }).map((_, i) => {
          const pIn = spring({ frame: frame - i * 3, fps, config: { damping: 200 }, durationInFrames: 14 });
          const dx = (i - 3) * 22 * (1 - collapse);
          const dy = -(i - 3) * 8 * (1 - collapse);
          const rot = (i - 3) * 3.6 * (1 - collapse);
          return (
            <div
              key={i}
              style={{
                position: 'absolute',
                opacity: (0.5 + 0.4 * (i / N)) * pIn * (1 - collapse),
                transform: `translate(${dx}px, ${dy}px) rotate(${rot}deg)`,
                zIndex: i,
              }}
            >
              <MemoryCard subject="john" relation="prefers" object='"window seat"' width={380} />
            </div>
          );
        })}

        {/* the surviving grain → supersedes down to history */}
        <div
          style={{
            position: 'absolute',
            opacity: survivorIn,
            transform: `translateY(${sup * 250}px) scale(${interpolate(sup, [0, 1], [1, 0.8])})`,
            zIndex: 20,
          }}
        >
          <MemoryCard subject="john" relation="prefers" object='"window seat"' width={380} status={sup > 0.35 ? 'stale' : 'current'} />
        </div>

        {/* the new current value slides in from above */}
        <div
          style={{
            position: 'absolute',
            opacity: newIn,
            transform: `translateY(${interpolate(newIn, [0, 1], [-250, 0])}px)`,
            zIndex: 30,
          }}
        >
          <MemoryCard subject="john" relation="prefers" object='"aisle seat"' width={380} status="current" />
        </div>

        {/* ✓ badge as the pile becomes one */}
        <div style={{ position: 'absolute', top: 34, right: 90, opacity: badge, transform: `scale(${interpolate(badge, [0, 1], [0.7, 1])})`, zIndex: 40 }}>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 12,
              padding: '12px 22px',
              borderRadius: 999,
              backgroundColor: theme.okBg,
              border: '1px solid #2E4A33',
              color: theme.green,
              fontFamily: theme.sans,
              fontSize: 30,
              fontWeight: 700,
            }}
          >
            ✓ 1 grain
          </div>
        </div>
      </div>
    </SceneFrame>
  );
};

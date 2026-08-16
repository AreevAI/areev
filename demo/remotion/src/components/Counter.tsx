import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';

// An animated number that counts from `from` to `to`.
export const Counter: React.FC<{
  to: number;
  from?: number;
  delay?: number;
  dur?: number;
  decimals?: number;
  prefix?: string;
  suffix?: string;
}> = ({ to, from = 0, delay = 0, dur = 30, decimals = 0, prefix = '', suffix = '' }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const p = spring({ frame: frame - delay, fps, config: { damping: 200 }, durationInFrames: dur });
  const v = interpolate(p, [0, 1], [from, to]);
  return (
    <>
      {prefix}
      {v.toFixed(decimals)}
      {suffix}
    </>
  );
};

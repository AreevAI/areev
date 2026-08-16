import React from 'react';
import { AbsoluteFill } from 'remotion';
import { TransitionSeries, linearTiming } from '@remotion/transitions';
import { fade } from '@remotion/transitions/fade';
import { theme } from './theme';
import { ColdOpen } from './scenes/ColdOpen';
import { RotScene } from './scenes/RotScene';
import { DedupScene } from './scenes/DedupScene';
import { DoesntRot } from './scenes/Beats';
import { GraphUI } from './scenes/GraphUI';
import { Provenance, OneLineCard } from './scenes/LearnMcp';
import { NoDelete } from './scenes/NoDelete';
import { Close } from './scenes/Close';

const T = 14; // cross-fade length

// Flow: hook → the rot (animated) → the fix (animated cards) → one terminal →
// graph UI → learn → no-delete → one line → close.
export const SCENES: { c: React.FC; d: number }[] = [
  { c: ColdOpen, d: 75 },
  { c: RotScene, d: 200 },
  { c: DedupScene, d: 230 },
  { c: DoesntRot, d: 300 },
  { c: GraphUI, d: 200 },
  { c: Provenance, d: 210 },
  { c: NoDelete, d: 145 },
  { c: OneLineCard, d: 130 },
  { c: Close, d: 150 },
];

export const TOTAL = SCENES.reduce((n, s) => n + s.d, 0) - (SCENES.length - 1) * T;

export const AreevDemo: React.FC = () => (
  <AbsoluteFill style={{ backgroundColor: theme.bg }}>
    <TransitionSeries>
      {SCENES.map(({ c: C, d }, i) => (
        <React.Fragment key={i}>
          {i > 0 && (
            <TransitionSeries.Transition presentation={fade()} timing={linearTiming({ durationInFrames: T })} />
          )}
          <TransitionSeries.Sequence durationInFrames={d}>
            <C />
          </TransitionSeries.Sequence>
        </React.Fragment>
      ))}
    </TransitionSeries>
  </AbsoluteFill>
);

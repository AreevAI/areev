import React from 'react';
import { AbsoluteFill } from 'remotion';
import { theme } from './theme';
import { AppWindow } from './components/AppWindow';
import { Graph } from './components/Graph';
import { MemoriesView } from './components/MemoriesView';

// Clean, caption-free console screenshots for the README (recreated from the
// shipped console.html design).
const Frame: React.FC<{ children: React.ReactNode }> = ({ children }) => (
  <AbsoluteFill style={{ backgroundColor: theme.bg, justifyContent: 'center', alignItems: 'center', padding: 56 }}>
    {children}
  </AbsoluteFill>
);

export const ShotGraph: React.FC = () => (
  <Frame>
    <AppWindow tabs={['memories', 'graph', 'query']} active={1} width={1808} height={968}>
      <div style={{ position: 'absolute', inset: 0, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        <Graph startAt={-90} />
      </div>
    </AppWindow>
  </Frame>
);

export const ShotMemories: React.FC = () => (
  <Frame>
    <AppWindow tabs={['memories', 'graph', 'query']} active={0} width={1808} height={968}>
      <MemoriesView />
    </AppWindow>
  </Frame>
);

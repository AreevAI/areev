import React from 'react';
import { SceneFrame, Hi } from '../components/SceneFrame';
import { AppWindow } from '../components/AppWindow';
import { Graph } from '../components/Graph';
import { theme } from '../theme';

// The web console's graph view — recreated from the shipped console.html design
// (Paper was unavailable this session).
export const GraphUI: React.FC = () => (
  <SceneFrame
    label="inspect it"
    caption={
      <>
        The whole memory, as a <Hi>graph</Hi>.
      </>
    }
  >
    <AppWindow tabs={['memories', 'graph', 'query']} active={1} width={1560} height={720}>
      <div style={{ position: 'absolute', inset: 0, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        <Graph startAt={8} />
      </div>
    </AppWindow>
  </SceneFrame>
);

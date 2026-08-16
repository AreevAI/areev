import React from 'react';
import { SceneFrame, Hi } from '../components/SceneFrame';
import { Terminal } from '../components/Terminal';
import { theme } from '../theme';

// Beat 1 — Add & recall
export const AddRecall: React.FC = () => (
  <SceneFrame
    label="01 · embedded"
    caption={
      <>
        Store a memory. Recall it — <Hi>in-process, microseconds</Hi>. No server, no embedding API, no network hop.
      </>
    }
  >
    <Terminal
      blocks={[
        {
          cmd: 'areev add john prefers "window seat"',
          out: [{ t: '13d150b4…71384c8', c: theme.sky }],
        },
        {
          cmd: 'areev recall john',
          out: [{ t: 'john · prefers · "window seat"', c: theme.text }],
          hold: 26,
        },
      ]}
    />
  </SceneFrame>
);

// Beat 2 — It doesn't rot
export const DoesntRot: React.FC = () => (
  <SceneFrame
    label="see it run"
    caption={
      <>
        Idempotent · supersede · history — <Hi c={theme.green}>for real</Hi>.
      </>
    }
  >
    <Terminal
      blocks={[
        {
          cmd: 'areev add john prefers "window seat" --idempotent',
          out: [{ t: '13d150b4…71384c8', c: theme.sky }],
          hold: 8,
        },
        {
          cmd: 'areev add john prefers "window seat" --idempotent',
          out: [
            { t: '13d150b4…71384c8', c: theme.sky },
            { t: '(unchanged — value already current, no new grain)', c: theme.green },
          ],
          hold: 10,
        },
        {
          cmd: 'areev cal \'SUPERSEDE sha256:13d150b4… SET object = "aisle seat"\'',
          out: [{ t: 'superseded ✓', c: theme.green }],
          hold: 8,
        },
        {
          cmd: 'areev history --subject john --relation prefers',
          out: [
            { t: '• aisle seat    ← current', c: theme.text, b: true },
            { t: '• window seat   kept, superseded', c: theme.dim },
          ],
          hold: 24,
        },
      ]}
    />
  </SceneFrame>
);

// Beat 4 — Safe for agents that learn
export const SafeToLearn: React.FC = () => (
  <SceneFrame
    label="04 · safe to learn"
    caption={
      <>
        In a learning loop, rot <Hi>compounds</Hi>. Every lesson links to the experience that taught it — trace it,
        revise it, <Hi>undo a bad session</Hi>. Memory safe to learn on.
      </>
    }
  >
    <Terminal
      title="areev — agent.db"
      blocks={[
        {
          cmd: 'areev remember --observer executor \\\n  --content "session 41: fixed the flaky test by isolating the tempdir"',
          out: [{ t: 'observation 80f2942b…d98903', c: theme.sky }],
          hold: 8,
        },
        {
          cmd: 'areev cal \'ADD fact … relation="lesson" … SET derived_from = "80f2942b…"\'',
          out: [{ t: 'lesson added ✓', c: theme.green }],
          hold: 8,
        },
        {
          cmd: 'areev provenance 80f2942b…d98903',
          out: [{ t: 'fix_flaky · lesson · "Isolate the shared tempdir per test."', c: theme.text }],
          hold: 26,
        },
      ]}
    />
  </SceneFrame>
);

// Beat 5 — One line of memory for an agent
export const OneLine: React.FC = () => (
  <SceneFrame
    label="05 · model-native"
    caption={
      <>
        One line gives any MCP client persistent memory — <Hi>recall injected automatically</Hi>, every turn captured.
        It's just MCP.
      </>
    }
  >
    <Terminal
      title="areev — claude-code"
      blocks={[
        {
          cmd: 'areev hook claude-code',
          out: [{ t: '→ prints hooks: recall-before-prompt  +  capture-on-stop', c: theme.dim }],
          hold: 10,
        },
        {
          cmd: 'claude mcp add areev -- areev serve --mcp --db ~/.areev/code.db',
          out: [{ t: '✓ areev connected — memory across sessions', c: theme.green }],
          hold: 26,
        },
      ]}
    />
  </SceneFrame>
);

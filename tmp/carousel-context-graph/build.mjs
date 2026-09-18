import fs from "node:fs/promises";
import path from "node:path";
import { Presentation, PresentationFile } from "@oai/artifact-tool";

const TMP_DIR = "/Users/sathish/mg/products/areev/tmp/carousel-context-graph";
const OUTPUT_DIR = "/Users/sathish/mg/products/areev/output";
const FINAL_PPTX = path.join(OUTPUT_DIR, "context-graph-vs-knowledge-graph-carousel.pptx");
const LOGO_PATH = "/Users/sathish/mg/products/areev/docs/assets/brand/areev-logo-light.png";
const DARK_LOGO_PATH = "/Users/sathish/mg/products/areev/docs/assets/brand/areev-logo-dark.png";
const MARK_PATH = "/Users/sathish/mg/products/areev/docs/assets/brand/areev-mark-512.png";
const GRAPH_SCREEN_PATH = "/Users/sathish/mg/products/areev/demo/screens/graph-light.png";
const GITHUB_MARK_PATH = "/Users/sathish/mg/products/areev/tmp/carousel-context-graph/github-mark.png";

const W = 1080;
const H = 1080;
const M = 72;
const FONT = "Helvetica Neue";
const COLORS = {
  white: "#FFFFFF",
  surface: "#F7F9FC",
  ink: "#111827",
  blue: "#2F72E8",
  blueSoft: "#EAF1FE",
  blueMid: "#BFD2F7",
  muted: "#6B7280",
  rule: "#E3E6ED",
  darkRule: "#B8C0CC",
  green: "#0A7D55",
  greenSoft: "#EAF7F1",
  amber: "#B45309",
  amberSoft: "#FFF4E5",
};

const SOURCES = {
  kg: "https://arxiv.org/html/2003.02320",
  contextPaper: "https://arxiv.org/abs/2406.11160",
  ibm: "https://www.ibm.com/think/topics/context-graph",
  foundation: "https://foundationcapital.com/ideas/context-graphs-ais-trillion-dollar-opportunity",
  w3c: "https://www.w3.org/groups/cg/context-graph/",
  areev: "https://github.com/AreevAI/areev/blob/main/README.md",
  githubMark: "https://github.githubassets.com/images/modules/logos_page/GitHub-Mark.png",
};

function box(slide, x, y, w, h, fill = COLORS.white, lineFill = COLORS.rule, lineWidth = 1.5, radius = 22) {
  return slide.shapes.add({
    geometry: "roundRect",
    position: { left: x, top: y, width: w, height: h },
    fill,
    line: { style: "solid", fill: lineFill, width: lineWidth },
    borderRadius: radius,
  });
}

function rect(slide, x, y, w, h, fill, lineFill = "none", lineWidth = 0) {
  return slide.shapes.add({
    geometry: "rect",
    position: { left: x, top: y, width: w, height: h },
    fill,
    line: { style: "solid", fill: lineFill, width: lineWidth },
  });
}

function line(slide, x, y, w, h = 0, color = COLORS.rule, width = 2) {
  return slide.shapes.add({
    geometry: "line",
    position: { left: x, top: y, width: w, height: h },
    fill: "none",
    line: { style: "solid", fill: color, width },
  });
}

function textBox(slide, text, x, y, w, h, size, color = COLORS.ink, opts = {}) {
  const shape = slide.shapes.add({
    geometry: "textbox",
    name: opts.name,
    position: { left: x, top: y, width: w, height: h },
    fill: "none",
    line: { style: "solid", fill: "none", width: 0 },
  });
  shape.text = text;
  shape.text.style = {
    fontSize: size,
    typeface: FONT,
    color,
    bold: opts.bold ?? false,
    alignment: opts.align ?? "left",
    verticalAlignment: opts.valign ?? "top",
    autoFit: opts.autoFit ?? "none",
    wrap: "square",
    lineSpacing: opts.lineSpacing,
    insets: opts.insets ?? { top: 0, right: 0, bottom: 0, left: 0 },
  };
  return shape;
}

function richText(slide, paragraphs, x, y, w, h, size, opts = {}) {
  const shape = slide.shapes.add({
    geometry: "textbox",
    name: opts.name,
    position: { left: x, top: y, width: w, height: h },
    fill: "none",
    line: { style: "solid", fill: "none", width: 0 },
  });
  shape.text.set(paragraphs);
  shape.text.style = {
    fontSize: size,
    typeface: FONT,
    color: opts.color ?? COLORS.ink,
    bold: opts.bold ?? false,
    alignment: opts.align ?? "left",
    verticalAlignment: opts.valign ?? "top",
    autoFit: opts.autoFit ?? "none",
    wrap: "square",
    lineSpacing: opts.lineSpacing,
    insets: opts.insets ?? { top: 0, right: 0, bottom: 0, left: 0 },
  };
  return shape;
}

function eyebrow(slide, label, x = M, y = 62, color = COLORS.blue) {
  textBox(slide, label.toUpperCase(), x, y, 680, 28, 22, color, { bold: true });
}

function slideNumber(slide, n, markBytes, color = COLORS.muted) {
  slide.images.add({
    blob: markBytes,
    contentType: "image/png",
    alt: "Areev mark",
    fit: "contain",
    position: { left: M, top: 1016, width: 28, height: 28 },
  });
  line(slide, 112, 1030, 56, 0, COLORS.blue, 4);
  textBox(slide, `${String(n).padStart(2, "0")} / 10`, 900, 1016, 108, 30, 21, color, {
    align: "right",
    bold: true,
  });
}

function title(slide, copy, opts = {}) {
  return textBox(slide, copy, M, opts.y ?? 104, 936, opts.h ?? 126, opts.size ?? 58, opts.color ?? COLORS.ink, {
    bold: true,
    lineSpacing: opts.lineSpacing ?? 0.94,
  });
}

function note(slide, urls, extra = "") {
  const rows = ["[Sources]", ...urls.map((u) => `- ${u}`)];
  if (extra) rows.push("", extra);
  slide.speakerNotes.textFrame.setText(rows.join("\n"));
  slide.speakerNotes.setVisible(false);
}

function labelCard(slide, x, y, w, h, label, body, accent = COLORS.blue, fill = COLORS.white) {
  box(slide, x, y, w, h, fill, COLORS.rule, 1.5, 20);
  rect(slide, x, y, 7, h, accent);
  textBox(slide, label.toUpperCase(), x + 28, y + 24, w - 52, 30, 22, accent, { bold: true });
  textBox(slide, body, x + 28, y + 70, w - 52, h - 92, 31, COLORS.ink, { bold: false, lineSpacing: 0.98 });
}

function smallNode(slide, x, y, w, h, label, body) {
  box(slide, x, y, w, h, COLORS.white, COLORS.rule, 1.5, 18);
  rect(slide, x, y, 7, h, COLORS.blue);
  textBox(slide, label.toUpperCase(), x + 28, y + 18, w - 46, 26, 19, COLORS.blue, { bold: true });
  textBox(slide, body, x + 28, y + 54, w - 46, 46, 25, COLORS.ink, { lineSpacing: 0.94 });
}

async function writeBlob(filePath, blob) {
  await fs.writeFile(filePath, new Uint8Array(await blob.arrayBuffer()));
}

async function build() {
  await fs.mkdir(OUTPUT_DIR, { recursive: true });
  const [logoBytes, darkLogoBytes, markBytes, graphBytes, githubBytes] = await Promise.all([
    fs.readFile(LOGO_PATH),
    fs.readFile(DARK_LOGO_PATH),
    fs.readFile(MARK_PATH),
    fs.readFile(GRAPH_SCREEN_PATH),
    fs.readFile(GITHUB_MARK_PATH),
  ]);

  const deck = Presentation.create({ slideSize: { width: W, height: H } });

  // 1 - Cover: sparse Codex Grid title composition.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.white;
    s.images.add({
      blob: logoBytes,
      contentType: "image/png",
      alt: "Areev",
      fit: "contain",
      position: { left: M, top: 58, width: 168, height: 56 },
    });
    textBox(s, "A PRACTICAL GUIDE FOR AI AGENTS", M, 142, 700, 30, 22, COLORS.blue, { bold: true });
    textBox(s, "Context Graph", M, 238, 936, 104, 86, COLORS.blue, { bold: true });
    textBox(s, "vs", M, 346, 220, 72, 56, COLORS.muted, { bold: false });
    textBox(s, "Knowledge Graph", M, 430, 936, 108, 86, COLORS.ink, { bold: true });
    line(s, M, 690, 936, 0, COLORS.rule, 2);
    textBox(
      s,
      "One maps what is known.\nThe other assembles what matters now.",
      M,
      740,
      820,
      150,
      38,
      COLORS.ink,
      { lineSpacing: 1.02 },
    );
    textBox(s, "Swipe →", M, 928, 220, 40, 28, COLORS.blue, { bold: true });
    slideNumber(s, 1, markBytes);
    note(s, [SOURCES.kg, SOURCES.ibm, SOURCES.areev], "The term context graph is emerging and is defined operationally in slide 2.");
  }

  // 2 - Paired questions.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.surface;
    eyebrow(s, "Start with the job");
    title(s, "They answer different questions.");
    box(s, M, 270, 444, 540, COLORS.white, COLORS.rule, 1.5, 24);
    box(s, 564, 270, 444, 540, COLORS.white, COLORS.blueMid, 2.5, 24);
    textBox(s, "KNOWLEDGE GRAPH", 108, 310, 372, 32, 22, COLORS.muted, { bold: true });
    textBox(s, "What exists - and how is it connected?", 108, 372, 360, 170, 42, COLORS.ink, { bold: true, lineSpacing: 0.95 });
    line(s, 108, 570, 324, 0, COLORS.rule, 2);
    textBox(s, "ENTITIES\nRELATIONSHIPS\nFACTS", 108, 615, 300, 126, 27, COLORS.muted, { bold: true, lineSpacing: 1.1 });

    textBox(s, "CONTEXT GRAPH", 600, 310, 372, 32, 22, COLORS.blue, { bold: true });
    textBox(s, "What matters now - for this task, policy, and time?", 600, 372, 360, 190, 42, COLORS.ink, { bold: true, lineSpacing: 0.95 });
    line(s, 600, 588, 324, 0, COLORS.blueMid, 2);
    textBox(s, "INTENT\nSTATE\nEVIDENCE + CONSTRAINTS", 600, 635, 340, 126, 27, COLORS.blue, { bold: true, lineSpacing: 1.1 });

    textBox(
      s,
      "'Context graph' is an emerging term. Here, it means the execution-time view an agent acts from.",
      M,
      850,
      936,
      88,
      26,
      COLORS.muted,
      { lineSpacing: 1.02 },
    );
    slideNumber(s, 2, markBytes);
    note(s, [SOURCES.kg, SOURCES.contextPaper, SOURCES.ibm, SOURCES.w3c]);
  }

  // 3 - Image-led knowledge graph definition.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.white;
    eyebrow(s, "The durable map");
    title(s, "Knowledge graphs make meaning durable.", { size: 56, h: 122 });
    textBox(s, "Entities + relationships + typed facts", M, 226, 760, 45, 31, COLORS.muted, { bold: true });
    box(s, M, 290, 936, 610, COLORS.surface, COLORS.rule, 1.5, 24);
    s.images.add({
      blob: graphBytes,
      contentType: "image/png",
      alt: "Areev knowledge graph showing connected people, companies, invoices, and processes",
      fit: "cover",
      crop: { left: 0.19, top: 0.08, right: 0.02, bottom: 0.14 },
      geometry: "roundRect",
      borderRadius: 20,
      position: { left: 92, top: 310, width: 896, height: 500 },
    });
    box(s, 110, 726, 360, 144, COLORS.white, COLORS.rule, 1.5, 18);
    textBox(s, "QUERY THE CONNECTIONS", 136, 750, 308, 30, 20, COLORS.blue, { bold: true });
    textBox(s, "Walk the graph.\nRewind the history.", 136, 792, 308, 64, 23, COLORS.ink, { bold: true, lineSpacing: 0.98 });
    textBox(s, "Good for integration, discovery, analytics, recommendations, and multi-hop reasoning.", M, 925, 936, 52, 27, COLORS.ink, { lineSpacing: 1.0 });
    slideNumber(s, 3, markBytes);
    note(s, [SOURCES.kg, SOURCES.areev, GRAPH_SCREEN_PATH]);
  }

  // 4 - Four-point grid.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.surface;
    eyebrow(s, "Where facts stop being enough");
    title(s, "An action depends on more than the domain map.", { h: 132, size: 56 });
    labelCard(s, M, 270, 444, 220, "Time", "Which version is valid now?", COLORS.blue, COLORS.white);
    labelCard(s, 564, 270, 444, 220, "Provenance", "Where did this belief come from?", COLORS.blue, COLORS.white);
    labelCard(s, M, 522, 444, 220, "State", "What happened in this run?", COLORS.blue, COLORS.white);
    labelCard(s, 564, 522, 444, 220, "Authority", "Who may approve this action?", COLORS.blue, COLORS.white);
    richText(
      s,
      [[
        { run: "The same fact can be ", textStyle: { color: COLORS.ink } },
        { run: "relevant", textStyle: { bold: true, color: COLORS.blue } },
        { run: ", stale, forbidden - or too costly to include.", textStyle: { color: COLORS.ink } },
      ]],
      M,
      802,
      920,
      96,
      34,
      { lineSpacing: 1.0 },
    );
    slideNumber(s, 4, markBytes);
    note(s, [SOURCES.kg, SOURCES.contextPaper, SOURCES.ibm, SOURCES.foundation]);
  }

  // 5 - Inclusion diagram: context graph around the knowledge graph.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.white;
    eyebrow(s, "The runtime view");
    title(s, "A context graph adds the situation around the facts.", { size: 54 });

    box(s, M, 265, 936, 625, COLORS.surface, COLORS.blueMid, 2.5, 28);
    textBox(s, "CONTEXT GRAPH", 108, 294, 300, 30, 22, COLORS.blue, { bold: true });
    textBox(s, "Execution-time activation layer", 680, 294, 288, 30, 21, COLORS.muted, { bold: true, align: "right" });

    box(s, 360, 448, 360, 152, COLORS.white, COLORS.ink, 2.5, 24);
    textBox(s, "KNOWLEDGE\nGRAPH", 394, 482, 292, 88, 38, COLORS.ink, { bold: true, align: "center", lineSpacing: 0.92 });

    smallNode(s, 100, 350, 250, 112, "Current task", "What is the goal?");
    smallNode(s, 730, 350, 250, 112, "History + time", "What changed?");
    smallNode(s, 100, 520, 250, 112, "Evidence", "Why believe it?");
    smallNode(s, 730, 520, 250, 112, "Workflow", "Where are we?");
    smallNode(s, 210, 690, 260, 112, "Permissions", "What is allowed?");
    smallNode(s, 610, 690, 260, 112, "Budget", "What can fit?");

    textBox(s, "Not a second graph database. A decision-ready view.", M, 930, 936, 42, 30, COLORS.ink, { bold: true, align: "center" });
    slideNumber(s, 5, markBytes);
    note(s, [SOURCES.contextPaper, SOURCES.ibm, SOURCES.foundation, SOURCES.areev]);
  }

  // 6 - Dense but readable comparison table.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.surface;
    eyebrow(s, "Side by side");
    title(s, "The difference is purpose - not storage technology.", { size: 53 });

    box(s, M, 252, 936, 672, COLORS.white, COLORS.rule, 1.5, 24);
    rect(s, M, 252, 936, 84, COLORS.ink);
    textBox(s, "LENS", 104, 278, 180, 30, 22, COLORS.white, { bold: true });
    textBox(s, "KNOWLEDGE GRAPH", 380, 278, 250, 30, 22, COLORS.white, { bold: true });
    textBox(s, "CONTEXT GRAPH", 720, 278, 230, 30, 22, COLORS.white, { bold: true });

    const rows = [
      ["Primary job", "Represent durable meaning", "Assemble situational relevance"],
      ["Typical unit", "Entity-relation-entity fact", "Contextualized fact, event, or trace"],
      ["Horizon", "Long-lived domain memory", "This task, turn, or run"],
      ["Edges emphasize", "Semantic relationships", "Temporal, causal, operational, policy"],
      ["Output", "Queryable subgraph", "Budget-shaped context for a decision"],
    ];
    const rowTop = 336;
    const rowH = 114;
    rows.forEach((r, i) => {
      const y = rowTop + i * rowH;
      if (i % 2 === 1) rect(s, 74, y, 932, rowH, COLORS.surface);
      if (i > 0) line(s, 74, y, 932, 0, COLORS.rule, 1.2);
      textBox(s, r[0].toUpperCase(), 104, y + 34, 210, 40, 20, COLORS.muted, { bold: true });
      textBox(s, r[1], 330, y + 26, 300, 62, 27, COLORS.ink, { bold: i === 4, lineSpacing: 0.96 });
      textBox(s, r[2], 690, y + 26, 286, 68, 27, i === 4 ? COLORS.blue : COLORS.ink, { bold: i === 4, lineSpacing: 0.96 });
    });
    textBox(s, "Both can live in the same underlying graph and store.", M, 946, 936, 36, 26, COLORS.muted, { align: "center", bold: true });
    slideNumber(s, 6, markBytes);
    note(s, [SOURCES.kg, SOURCES.contextPaper, SOURCES.ibm]);
  }

  // 7 - Illustrative invoice decision.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.white;
    eyebrow(s, "Illustrative example");
    title(s, "Should the agent pay this invoice?");

    textBox(s, "THE KNOWLEDGE GRAPH KNOWS", M, 250, 420, 30, 21, COLORS.muted, { bold: true });
    textBox(s, "THE CONTEXT GRAPH ADDS", 564, 250, 420, 30, 21, COLORS.blue, { bold: true });

    box(s, M, 300, 444, 500, COLORS.surface, COLORS.rule, 1.5, 24);
    box(s, 564, 300, 444, 500, COLORS.blueSoft, COLORS.blueMid, 2, 24);

    const left = ["Acme is an approved vendor", "Invoice total is $24,700", "Dev is the approver", "Approval limit is $25,000"];
    const right = ["Bank details changed yesterday", "A prior run flagged a mismatch", "Dev's credential is valid now", "The workflow is waiting on review"];
    left.forEach((t, i) => {
      const y = 346 + i * 102;
      textBox(s, "•", 106, y, 28, 40, 34, COLORS.muted, { bold: true });
      textBox(s, t, 146, y + 2, 330, 62, 29, COLORS.ink, { lineSpacing: 0.98 });
      if (i < 3) line(s, 110, y + 74, 330, 0, COLORS.rule, 1.2);
    });
    right.forEach((t, i) => {
      const y = 346 + i * 102;
      textBox(s, "→", 598, y - 2, 34, 40, 32, COLORS.blue, { bold: true });
      textBox(s, t, 646, y + 2, 326, 62, 29, COLORS.ink, { lineSpacing: 0.98 });
      if (i < 3) line(s, 602, y + 74, 330, 0, COLORS.blueMid, 1.2);
    });

    rect(s, M, 842, 936, 112, COLORS.ink);
    textBox(s, "Same knowledge. Different action: hold and escalate.", 112, 874, 856, 50, 32, COLORS.white, { bold: true, align: "center" });
    slideNumber(s, 7, markBytes, COLORS.muted);
    note(s, [SOURCES.kg, SOURCES.ibm, SOURCES.areev], "Illustrative scenario. It demonstrates how operational context can change an action without changing the underlying facts.");
  }

  // 8 - Process sequence: recall to assembled context.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.surface;
    eyebrow(s, "From memory to action");
    title(s, "Retrieval finds candidates. Context assembly makes the call.", { size: 52, h: 132 });

    const steps = [
      ["01", "RECALL", "Lexical, semantic, structural, graph"],
      ["02", "FILTER", "Time, policy, provenance, permissions"],
      ["03", "RANK", "Task relevance, priority, diversity"],
      ["04", "SHAPE", "Fit the model's token budget"],
    ];
    steps.forEach((st, i) => {
      const y = 270 + i * 158;
      if (i < steps.length - 1) {
        line(s, 136, y + 120, 0, 50, COLORS.blueMid, 3);
        textBox(s, "↓", 118, y + 126, 36, 42, 28, COLORS.blue, { bold: true, align: "center" });
      }
      box(s, M, y, 936, 120, COLORS.white, i === 3 ? COLORS.blue : COLORS.rule, i === 3 ? 2.5 : 1.5, 22);
      box(s, 104, y + 26, 82, 68, i === 3 ? COLORS.blue : COLORS.surface, i === 3 ? COLORS.blue : COLORS.rule, 1.5, 16);
      textBox(s, st[0], 104, y + 42, 82, 32, 24, i === 3 ? COLORS.white : COLORS.muted, { bold: true, align: "center" });
      textBox(s, st[1], 222, y + 26, 190, 30, 22, i === 3 ? COLORS.blue : COLORS.ink, { bold: true });
      textBox(s, st[2], 222, y + 62, 710, 36, 29, COLORS.ink, { lineSpacing: 1.0 });
    });
    rect(s, M, 900, 936, 66, COLORS.blue);
    textBox(s, "MODEL-READY CONTEXT → ACTION → NEW EVIDENCE", 104, 918, 872, 34, 25, COLORS.white, { bold: true, align: "center" });
    slideNumber(s, 8, markBytes);
    note(s, [SOURCES.ibm, SOURCES.areev]);
  }

  // 9 - Practical selection rule.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.white;
    eyebrow(s, "A practical rule");
    title(s, "Use both when the system can act.");

    box(s, M, 270, 444, 510, COLORS.surface, COLORS.rule, 1.5, 24);
    box(s, 564, 270, 444, 510, COLORS.blueSoft, COLORS.blueMid, 2, 24);

    textBox(s, "SEARCH + DISCOVERY", 108, 312, 340, 28, 21, COLORS.muted, { bold: true });
    textBox(s, "A knowledge graph may be enough.", 108, 370, 350, 105, 40, COLORS.ink, { bold: true, lineSpacing: 0.95 });
    textBox(s, "Catalogs\nEnterprise search\nAnalytics\nRecommendations", 108, 530, 310, 190, 30, COLORS.muted, { lineSpacing: 1.16 });

    textBox(s, "DECISIONS + ACTIONS", 600, 312, 340, 28, 21, COLORS.blue, { bold: true });
    textBox(s, "Add a context graph.", 600, 370, 350, 105, 40, COLORS.ink, { bold: true, lineSpacing: 0.95 });
    textBox(s, "Agents\nWorkflow automation\nApprovals\nTriggers + tools", 600, 530, 310, 190, 30, COLORS.blue, { lineSpacing: 1.16, bold: true });

    textBox(s, "The higher the consequence, the more context must be explicit.", M, 842, 936, 90, 38, COLORS.ink, { bold: true, align: "center", lineSpacing: 0.98 });
    slideNumber(s, 9, markBytes);
    note(s, [SOURCES.ibm, SOURCES.foundation, SOURCES.areev]);
  }

  // 10 - Close.
  {
    const s = deck.slides.add();
    s.background.fill = COLORS.ink;
    s.images.add({
      blob: darkLogoBytes,
      contentType: "image/png",
      alt: "Areev",
      fit: "contain",
      position: { left: M, top: 58, width: 168, height: 56 },
    });
    textBox(s, "THE TAKEAWAY", M, 160, 400, 30, 22, "#79A8FA", { bold: true });
    textBox(s, "Store facts as a", M, 240, 936, 72, 62, COLORS.white, { bold: true });
    textBox(s, "knowledge graph.", M, 316, 936, 88, 72, "#B8C0CC", { bold: true });
    textBox(s, "Run agents on a", M, 466, 936, 72, 62, COLORS.white, { bold: true });
    textBox(s, "context graph.", M, 542, 936, 88, 72, "#79A8FA", { bold: true });
    line(s, M, 700, 936, 0, "#334155", 2);
    textBox(s, "See the implementation. Run the demo.", M, 742, 900, 48, 30, COLORS.white, { bold: true });
    box(s, M, 814, 936, 148, COLORS.white, COLORS.blue, 2.5, 24);
    s.images.add({
      blob: githubBytes,
      contentType: "image/png",
      alt: "GitHub mark",
      fit: "contain",
      position: { left: 106, top: 852, width: 72, height: 72 },
    });
    textBox(s, "★ Star Areev on GitHub", 208, 838, 740, 48, 34, COLORS.ink, { bold: true });
    textBox(s, "github.com/AreevAI/areev", 208, 900, 740, 34, 23, COLORS.blue, { bold: true });
    slideNumber(s, 10, markBytes, "#94A3B8");
    note(s, [SOURCES.kg, SOURCES.ibm, SOURCES.areev, SOURCES.githubMark]);
  }

  const renderDir = path.join(TMP_DIR, "renders");
  await fs.mkdir(renderDir, { recursive: true });
  for (const [i, slide] of deck.slides.items.entries()) {
    const stem = `slide-${String(i + 1).padStart(2, "0")}`;
    const png = await deck.export({ slide, format: "png", scale: 1 });
    await writeBlob(path.join(renderDir, `${stem}.png`), png);
    const layout = await slide.export({ format: "layout" });
    await fs.writeFile(path.join(renderDir, `${stem}.layout.json`), await layout.text());
  }
  const montage = await deck.export({ format: "webp", montage: true, scale: 1 });
  await writeBlob(path.join(TMP_DIR, "carousel-montage.webp"), montage);

  const pptx = await PresentationFile.exportPptx(deck);
  await pptx.save(FINAL_PPTX);
  console.log(FINAL_PPTX);
}

build().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});

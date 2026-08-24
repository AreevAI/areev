#!/usr/bin/env node --experimental-strip-types
/* invoice -> accounting: the whole agent, one file, embedded Areev.
 *
 * The TypeScript twin of ../python/agent.py -- same subcommands, same
 * fixtures, same assertions (../smoke.sh and ../improve.sh drive both).
 *
 * Two subcommands are subprocess seams the Areev runtime spawns (JSON on
 * stdin, JSON on stdout, one process per invocation) and never open the
 * memory: `tools` ($AREEV_TOOL_NAME picks one) and `connector`.
 * Everything else is the driver, embedding Areev in-process via the
 * `@areev/areev` napi binding (set $AREEV_JS to a checkout of
 * crates/areev-js to run against the tree instead of the npm release).
 *
 * Node's binding is promise-based and needs an explicit close() -- the
 * memory's exclusive lock is released there, not on garbage collection.
 */
import { createHash } from "node:crypto";
import { readFileSync, appendFileSync, mkdirSync, readdirSync } from "node:fs";
import { createRequire } from "node:module";
import * as path from "node:path";
import * as process from "node:process";

const HERE = path.dirname(new URL(import.meta.url).pathname);
const EXAMPLE = path.dirname(HERE);
const FIXTURES = process.env.MAIL_FIXTURES ?? path.join(EXAMPLE, "fixtures", "mail");
const MAIL_UPTO = process.env.MAIL_UPTO ?? "03"; // the acts advance this "clock"
const OUT = process.env.AGENT_OUT ?? path.join(HERE, "out");
const DB = process.env.AGENT_DB ?? path.join(OUT, "agent.db");
const SHEET = path.join(OUT, "sheet.jsonl");
const OUTBOX = path.join(OUT, "outbox.jsonl");

const NS = "org.ops"; // triggers, plan, tool definitions, journals, raw mail
const DESK = "agent:ap-desk"; // the agent's own principal -- it can never approve
const DESK_FROM = "ap-desk@desk.example";

// One mailbox per client; the client's knowledge lives under org.<client>.
const MAILBOXES: Record<string, string> = {
  acme: "ap-acme@desk.example",
  brightco: "ap-brightco@desk.example",
};
const APPROVER: Record<string, string> = {
  acme: "dana@acme.example",
  brightco: "priya@brightco.example",
};

// Pinned so every language's seeder mints the SAME content addresses --
// created_at is part of a grain's bytes, and a grain is its bytes.
const EPOCH_MS = 1756000000000;

const CONFIDENCE_FLOOR = 0.75;
const DEFAULT_THRESHOLD = 2500.0;

type Json = Record<string, unknown>;

const sorted = (obj: unknown): unknown =>
  Array.isArray(obj) ? obj.map(sorted)
  : obj && typeof obj === "object"
    ? Object.fromEntries(Object.entries(obj).sort().map(([k, v]) => [k, sorted(v)]))
    : obj;
const emit = (obj: unknown) => process.stdout.write(JSON.stringify(sorted(obj)) + "\n");
const append = (file: string, obj: unknown) => {
  mkdirSync(path.dirname(file), { recursive: true });
  appendFileSync(file, JSON.stringify(sorted(obj)) + "\n");
};
const marker = (messageId: string) =>
  createHash("sha256").update(messageId).digest("hex").slice(0, 12);
const readStdin = () => JSON.parse(readFileSync(0, "utf8"));

// ── the tools seam ─────────────────────────────────────────────────────────
// stdin is the run's merged state. On the trigger path the email is under
// "item" and the trigger's declared context under "context".

function walkGrains(node: unknown, out: Json[]): Json[] {
  if (Array.isArray(node)) node.forEach((v) => walkGrains(v, out));
  else if (node && typeof node === "object") {
    const d = node as Json;
    if ("relation" in d && "subject" in d) out.push(d);
    Object.values(d).forEach((v) => walkGrains(v, out));
  }
  return out;
}

function toolMain(): number {
  const state = readStdin();
  const item: Json = state.item ?? state;
  const grains = walkGrains(state.context ?? {}, []);
  const tool = process.env.AREEV_TOOL_NAME ?? "";

  if (tool === "parse_attachments") {
    // A photographed invoice has no text layer. Failing loudly is the
    // correct behaviour: a silent empty extraction posts a blank row.
    if (item.scanned) {
      process.stderr.write("pdftotext produced 0 characters - attachment is a scanned image\n");
      return 1;
    }
    emit({ texts: [{ filename: item.attachment ?? "invoice.pdf", chars: 4180 }] });
  } else if (tool === "extract_rows") {
    // The real one sends the PDF text to a model. This one reads the
    // fixture's own fields -- deterministic -- but applies the same memory
    // the real one would: an alias fact from a past correction
    // canonicalizes the vendor and settles the confidence question.
    let vendor = String(item.vendor ?? "unknown");
    let confidence = Number(item.confidence ?? 0.95);
    const aliases = new Map(
      grains.filter((g) => g.relation === "mg:alias_of")
        .map((g) => [String(g.subject), String(g.object)]),
    );
    if (aliases.has(vendor)) {
      vendor = aliases.get(vendor)!;
      confidence = Math.max(confidence, 0.95);
    }
    emit({
      rows: 1, vendor, amount: item.amount ?? 0, currency: item.currency ?? "USD",
      category: item.category ?? "Software", field_confidence: confidence,
      client: item.client ?? "unknown", message_id: item.message_id ?? "?",
      thread: item.thread ?? "?", sender: item.sender ?? "?",
    });
  } else if (tool === "validate_rows") {
    // The threshold is a fact in org.<client>, delivered through the
    // trigger's declared context -- not a constant in this script.
    const client = state.client ?? "unknown";
    let threshold = DEFAULT_THRESHOLD;
    for (const g of grains)
      if (g.relation === "review_threshold_usd" && g.subject === client)
        threshold = Number(g.object);
    const amount = Number(state.amount ?? 0);
    const confidence = Number(state.field_confidence ?? 1.0);
    const needsReview = amount >= threshold || confidence < CONFIDENCE_FLOOR;
    emit({
      needs_review: needsReview,
      row_key: `${state.message_id ?? "?"}#0`,
      review_reason: amount >= threshold ? "amount at or above threshold"
        : confidence < CONFIDENCE_FLOOR ? "field confidence below floor" : "clear",
    });
  } else if (tool === "send_ask") {
    // Always the client's approver, never the external sender. The marker
    // in the subject is how a reply finds its run again.
    const client = String(state.client ?? "unknown");
    append(OUTBOX, {
      to: APPROVER[client] ?? "unknown",
      subject: `Approve this expense: ${state.vendor} ${state.amount} ${state.currency} ` +
        `[areev:ap/${marker(String(state.message_id ?? "?"))}]`,
      vendor: state.vendor, amount: state.amount, reason: state.review_reason,
      reply_with: "approve | reject | revise + `Field: value` lines",
    });
    emit({ ask_sent: true });
  } else if (tool === "apply_corrections") {
    // Merge the approver's Field: value lines, mark them settled, and go
    // back around to re-ask -- the plan bounds this cycle with max_cycles.
    const merged: Json = { field_confidence: 1.0, revised: true };
    for (const [field, value] of Object.entries((state.corrections ?? {}) as Json)) {
      if (["vendor", "currency", "category"].includes(field)) merged[field] = String(value);
      else if (field === "amount") merged[field] = Number(value);
    }
    emit(merged);
  } else if (tool === "append_sheet") {
    const row = {
      row_key: state.row_key, client: state.client, vendor: state.vendor,
      amount: state.amount, currency: state.currency, category: state.category,
      approved_by: state.responder ?? "auto",
    };
    append(SHEET, row);
    emit({ appended: 1, row_key: row.row_key });
  } else if (tool === "reply_email") {
    append(OUTBOX, {
      to: state.sender ?? "?", subject: `Re: ${state.message_id ?? "?"}`,
      outcome: state.decision === "reject" ? "rejected" : "posted",
    });
    emit({ sent: true });
  } else {
    process.stderr.write(`unknown tool: ${tool}\n`);
    return 1;
  }
  return 0;
}

// ── the connector seam ─────────────────────────────────────────────────────
// The contract from docs/triggers.md: an ABSENT cursor means "seed and fire
// nothing", so declaring a trigger never replays mailbox history.

function connectorMain(): number {
  const req = readStdin();
  const mailbox = String(req.scope ?? "").replace(/^mailbox:/, "");
  const client = Object.keys(MAILBOXES).find((c) => MAILBOXES[c] === mailbox);
  const names = client
    ? readdirSync(path.join(FIXTURES, client))
        .filter((n) => n.endsWith(".json") && n.slice(0, 2) <= MAIL_UPTO).sort()
    : [];
  if (req.cursor == null) {
    emit({ items: [], cursor: "0", more: false });
    return 0;
  }
  const consumed = parseInt(req.cursor, 10);
  const page = names.slice(consumed, consumed + (req.max_items ?? 100));
  const items = page.map((name) => {
    const payload = JSON.parse(readFileSync(path.join(FIXTURES, client!, name), "utf8"));
    return { id: payload.message_id, payload };
  });
  emit({
    items, cursor: String(consumed + items.length),
    more: consumed + items.length < names.length,
  });
  return 0;
}

// ── the driver ─────────────────────────────────────────────────────────────

function openDb(actor: string = DESK) {
  // require(), not import: the binding is a CJS napi addon, and $AREEV_JS
  // lets CI run the tree's build instead of the npm release.
  const require = createRequire(import.meta.url);
  const { Areev } = require(process.env.AREEV_JS ?? "@areev/areev");
  mkdirSync(OUT, { recursive: true });
  return new Areev(DB, NS, null, actor);
}

const selfCmd = (sub: string) =>
  `${process.execPath} --experimental-strip-types ${path.join(HERE, "agent.mts")} ${sub}`;

async function seed(): Promise<number> {
  const db = openDb();
  const toolDef = (name: string, description: string, executorKind?: string) =>
    db.add("tool", JSON.stringify({
      tool_name: name, kind: "definition", tool_description: description,
      created_at: EPOCH_MS, ...(executorKind ? { executor_kind: executorKind } : {}),
    }), NS);

  const parse = await toolDef("parse_attachments", "pull the text layer out of each attachment");
  const extract = await toolDef("extract_rows", "turn invoice text into expense rows");
  const validate = await toolDef("validate_rows", "decide whether a person has to look");
  const ask = await toolDef("send_ask", "email the client's approver, with a marker");
  const review = await toolDef("human_review", "a person decides: approve, revise, or reject", "client");
  const corrections = await toolDef("apply_corrections", "merge the approver's Field: value lines");
  const sheet = await toolDef("append_sheet", "append the approved row to the expense sheet");
  const reply = await toolDef("reply_email", "tell the sender what happened");

  const wf = await db.add("workflow", JSON.stringify({
    name: "invoice-to-accounting",
    nodes: ["parse_attachments", "extract_rows", "validate_rows", "send_ask",
      "human_review", "apply_corrections", "append_sheet", "reply_done", "reply_rejected"],
    edges: [
      { src: "parse_attachments", dst: "extract_rows" },
      { src: "extract_rows", dst: "validate_rows" },
      { src: "validate_rows", dst: "append_sheet", cond: "needs_review == false" },
      { src: "validate_rows", dst: "send_ask", cond: "needs_review == true" },
      { src: "send_ask", dst: "human_review" },
      { src: "human_review", dst: "append_sheet", cond: 'decision == "approve"' },
      { src: "human_review", dst: "apply_corrections", cond: 'decision == "revise"' },
      { src: "human_review", dst: "reply_rejected", cond: 'decision == "reject"' },
      // The correction cycle: revise -> merge -> re-ask, at most 3 times.
      { src: "apply_corrections", dst: "send_ask", max_cycles: 3 },
      { src: "append_sheet", dst: "reply_done" },
    ],
    bindings: {
      parse_attachments: parse, extract_rows: extract, validate_rows: validate,
      send_ask: ask, human_review: review, apply_corrections: corrections,
      append_sheet: sheet,
      // Two nodes, one tool: both replies are the same effect.
      reply_done: reply, reply_rejected: reply,
    },
    retries: { extract_rows: 1 },
    created_at: EPOCH_MS,
  }), NS);

  await db.add("skill", JSON.stringify({
    name: "invoice-triage",
    description: "how this desk reads an invoice",
    instructions: "Extract one row per invoice. Prefer the canonical vendor " +
      "name from the alias facts. Never guess an amount: a " +
      "low-confidence field goes to review, not to the sheet.",
    created_at: EPOCH_MS,
  }), NS);

  // Client knowledge lives under org.<client> -- exact namespaces to write,
  // a "org.*" prefix to read the whole desk in one query.
  await db.addFact("acme", "review_threshold_usd", "2500", null, "org.acme", true);
  await db.addFact("brightco", "review_threshold_usd", "2500", null, "org.brightco", true);
  await db.addFact("Meridian Freight", "payment_terms", "net_30", null, "org.acme.vendors", true);
  await db.addFact("Cobalt Cloud", "payment_terms", "net_45", null, "org.brightco.vendors", true);

  // Retrieval + presentation ship IN the file as saved queries/templates
  // (qry:/tpl: meta rows) -- they replicate with the memory and are what
  // the triggers below name as declared context.
  await db.cal('DEFINE TEMPLATE vendor_line AS ' +
    '"- {{subject}} {{relation}} {{object}} ({{confidence}})"');
  await db.cal('DEFINE QUERY "extract_ctx"($session) ' +
    'DESCRIPTION "what extraction should know before reading an invoice" ' +
    'AS { ASSEMBLE "extract_ctx" FROM ' +
    'instructions: (RECALL skills LIMIT 2), ' +
    'desk: (RECALL facts WHERE namespace = "org.*" LIMIT 120), ' +
    'thread: (RECALL events WHERE session_id = $session RECENT 10) ' +
    'BUDGET 4000 tokens FORMAT json }');
  await db.cal('DEFINE QUERY "desk_pulse"() ' +
    'DESCRIPTION "the desk briefing itself: plan, tools, lessons, outcomes" ' +
    'AS { ASSEMBLE "desk_pulse" FROM ' +
    'plan: (RECALL workflows LIMIT 3), ' +
    'tools: (RECALL tools WHERE kind = "definition" LIMIT 12), ' +
    'activity: (RECALL tools WHERE kind != "definition" RECENT 40), ' +
    'lessons: (RECALL facts WHERE namespace = "org.*" LIMIT 40) ' +
    'BUDGET 2500 tokens FORMAT markdown }');

  // Egress anonymization starts in audit mode on the client subtrees --
  // measure before you rewrite. NEVER on org.ops: the rewriter would
  // mangle the operational JSON (dates, 64-char hashes) that lives there.
  await db.setAnonPolicy("org.acme", '{"mode": "audit"}');
  await db.setAnonPolicy("org.brightco", '{"mode": "audit"}');

  const triggers: Json = {};
  for (const client of Object.keys(MAILBOXES).sort()) {
    triggers[client] = await db.triggerAdd(JSON.stringify({
      kind: "polling", connector: "mock",
      scope: `mailbox:${MAILBOXES[client]}`, interval_secs: 1,
      workflow: wf, dedup_key: ["/message_id"],
      context_query: "extract_ctx($session = /thread)",
    }), `poll the ${client} AP mailbox for invoices`, NS);
  }

  emit({ workflow: wf, triggers });
  db.close();
  return 0;
}

async function ingest(): Promise<number> {
  const db = openDb();
  const report = JSON.parse(await db.triggerRun(
    null, null, null, null, selfCmd("connector"), selfCmd("tools"),
    null, null, null, null, null, null, 2_000_000, 300_000, 3600));
  emit(report);
  db.close();
  return 0;
}

type Ask = { runId: string; askId: string; state: Json };

async function pendingAsks(db: any): Promise<Ask[]> {
  const out: Ask[] = [];
  for (const runId of JSON.parse(await db.runList(100))) {
    const inspect = JSON.parse(await db.runInspect(runId));
    if (inspect.phase !== "open") continue;
    for (const [askId, entry] of Object.entries(inspect.pending_asks ?? {}))
      out.push({ runId, askId, state: (entry as Json).ask?.["input"] ?? {} });
  }
  return out;
}

async function asks(): Promise<number> {
  const db = openDb();
  const rows = (await pendingAsks(db)).map(({ runId, askId, state }) => {
    const item: Json = (state.item as Json) ?? state;
    return {
      run_id: runId, ask: askId, marker: marker(String(item.message_id ?? "?")),
      vendor: state.vendor, amount: state.amount, reason: state.review_reason,
    };
  });
  emit(rows);
  db.close();
  return 0;
}

const CUTOFF = /^On .* wrote:$|^-+ ?Original Message|^From: /;
const FIELD = /^(Vendor|Amount|Currency|Category):\s*(.+)$/i;

/** Deterministic reply reading: verb first, then Field: value lines.
 * Quoted history is cut, so a reply that quotes the ask does not
 * re-approve itself. */
function classify(body: string): Json | null {
  let verb: string | null = null;
  const corrections: Json = {};
  for (const raw of body.split("\n")) {
    const line = raw.trim();
    if (line.startsWith(">") || CUTOFF.test(line)) break;
    if (!line) continue;
    const m = FIELD.exec(line);
    if (m) corrections[m[1].toLowerCase()] = m[2].trim();
    else if (verb === null) verb = line.split(/\s+/)[0].toLowerCase();
  }
  if (verb === "reject") return { decision: "reject" };
  if (verb === "revise" || Object.keys(corrections).length)
    return { decision: "revise", corrections };
  if (verb === "approve") return { decision: "approve" };
  return null;
}

async function reply(file: string): Promise<number> {
  const mail = JSON.parse(readFileSync(file, "utf8"));
  const sender = String(mail.from ?? "?");
  const principal = sender === DESK_FROM ? DESK : "user:" + sender.split("@")[0];
  const ref = /\[areev:ap\/([0-9a-f]{12})\]/.exec(String(mail.subject ?? ""));
  const verdict = classify(String(mail.body ?? ""));
  if (!ref || !verdict) {
    process.stderr.write("unclassified reply -- left unactioned, a person reads it\n");
    return 3;
  }

  const db = openDb();
  try {
    for (const { runId, askId, state } of await pendingAsks(db)) {
      const item: Json = (state.item as Json) ?? state;
      if (marker(String(item.message_id ?? "?")) !== ref[1]) continue;
      const result = { ...verdict, responder: principal };
      try {
        await db.runRespond(runId, askId, JSON.stringify(result), principal);
      } catch (e) {
        process.stderr.write(`respond refused: ${e}\n`);
        return 4;
      }
      // A correction the approver then approved is a lesson worth keeping:
      // record the alias where the client's knowledge lives, and record
      // the correction itself as a tool outcome the loop can cluster.
      if (verdict.decision === "approve" && state.revised && state.vendor !== item.vendor)
        await db.addFact(String(item.vendor), "mg:alias_of", String(state.vendor),
          null, `org.${state.client ?? "unknown"}.vendors`, true);
      for (const field of Object.keys((verdict.corrections as Json) ?? {}))
        await db.recordToolCall("extract_rows", `corr:${field}:${state.client ?? "?"}`,
          true, String(item.thread ?? ""), null, null, runId);
      const outcome = JSON.parse(await db.runResume(runId, selfCmd("tools")));
      emit({ run_id: runId, decision: verdict.decision, responder: principal, outcome });
      return 0;
    }
    process.stderr.write(`no parked run matches marker ${ref[1]}\n`);
    return 5;
  } finally {
    db.close();
  }
}

async function improve(): Promise<number> {
  const db = openDb();
  // Tune the analyzers to this desk's volume: at ~4 invoices a week the
  // stock "half of all runs failed" bar would stay silent for a quarter.
  await db.setAnalyzerConfig("loop.run_outcome/1", true,
    JSON.stringify({ min_failure_ratio: 0.4 }));
  // Optional LLM reflection (DISCOVER->GROUND->VERIFY) on top of the
  // deterministic floor: LOOP_LLM_CMD names any --llm-cmd backend (see
  // examples/llm/); every model finding is grounded in grains first.
  const report = JSON.parse(await db.loopRun(
    null, null, null, null, process.env.LOOP_LLM_CMD ?? null));
  const recs = JSON.parse(await db.recommendations('{"status": "pending"}'));
  emit({
    loop: report,
    pending: recs.map((r: Json) => ({
      hash: r.hash, severity: r.severity, summary: r.summary,
      analyzer: r.analyzer, target: r.target_ref,
    })),
  });
  db.close();
  return 0;
}

async function decide(argv: string[]): Promise<number> {
  const [recPrefix, action] = argv;
  let because: string | null = null, actor: string | null = null;
  for (let i = 2; i < argv.length; i++) {
    if (argv[i] === "--because") because = argv[++i];
    if (argv[i] === "--as") actor = argv[++i];
  }
  if (!recPrefix || !action) {
    process.stderr.write("usage: decide <rec> approve|apply|dismiss --because ... --as user:X\n");
    return 2;
  }
  if (!because) {
    process.stderr.write("a decision with no written reason is refused\n");
    return 2;
  }
  const db = openDb(actor ?? "user:anonymous");
  try {
    const rec = JSON.parse(await db.recommendations(null))
      .map((r: Json) => String(r.hash))
      .find((h: string) => h.startsWith(recPrefix)) ?? recPrefix;
    if (action === "approve") console.log(await db.approveRecommendation(rec, because));
    else if (action === "apply") console.log(await db.applyRecommendation(rec, because));
    else if (action === "dismiss") console.log(await db.dismissRecommendation(rec, because));
    else { process.stderr.write(`unknown action ${action}\n`); return 2; }
    return 0;
  } catch (e) {
    process.stderr.write(`refused: ${e}\n`);
    return 4;
  } finally {
    db.close();
  }
}

async function teach(argv: string[]): Promise<number> {
  if (argv.length !== 4) {
    process.stderr.write("usage: teach NS SUBJECT RELATION OBJECT\n");
    return 2;
  }
  const [ns, subject, relation, object] = argv;
  const db = openDb();
  console.log(await db.addFact(subject, relation, object, null, ns, true));
  db.close();
  return 0;
}

async function brief(): Promise<number> {
  const db = openDb();
  console.log(await db.cal('RUN "desk_pulse"()'));
  console.log(await db.cal('RECALL facts WHERE namespace = "org.*" LIMIT 20 ' +
    'FORMAT TEMPLATE vendor_line'));
  db.close();
  return 0;
}

async function runs(): Promise<number> {
  // Outcome the same way `areev run list` derives it: the run-terminal
  // Observation the runtime writes in agent:harness.
  const db = openDb();
  const obs = JSON.parse(await db.cal(
    'RECALL observations WHERE namespace = "agent:harness" RECENT 200 FORMAT json'));
  const outcome = new Map<string, string>();
  for (const g of obs.grains ?? []) {
    const f = g.fields ?? {};
    if (f.observation_kind === "run_outcome") outcome.set(f.run_id, f.object);
  }
  emit(JSON.parse(await db.runList(100)).map((r: string) => ({
    run_id: r, outcome: outcome.get(r) ?? "open",
  })));
  db.close();
  return 0;
}

async function main(): Promise<number> {
  const [cmd, ...rest] = process.argv.slice(2);
  switch (cmd) {
    case "tools": return toolMain();
    case "connector": return connectorMain();
    case "seed": return seed();
    case "ingest": return ingest();
    case "asks": return asks();
    case "reply": return reply(rest[0]);
    case "improve": return improve();
    case "decide": return decide(rest);
    case "teach": return teach(rest);
    case "brief": return brief();
    case "runs": return runs();
    default:
      process.stderr.write("usage: agent.mts tools|connector|seed|ingest|asks|reply|improve|decide|teach|brief|runs\n");
      return 2;
  }
}

process.exit(await main());

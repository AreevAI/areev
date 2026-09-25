//! Decision-backend phase 3 (rows A2/A3) on the context path.
//!
//! Every test drives `ContextAssembler::with_decider` with a scripted,
//! deterministic in-process `DecisionBackend` — no network, no clock in any
//! assertion. What is pinned:
//!
//! - no decider (and a decider that always fails) renders byte-for-byte what
//!   today's rules render, with `decision: None`;
//! - a CALIBRATED backend omits off-topic candidates regardless of budget and
//!   lets a "summary would lose it" candidate take Full past the 70% line;
//! - an UNCALIBRATED backend reorders the allocation and never omits
//!   (proposal §2 rule 2);
//! - the intent `choice` replaces the keyword lists, the hint flags still
//!   win, and an error falls back to the keywords;
//! - a large candidate set splits into several requests under the state cap;
//! - provenance is populated.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use areev_cal::store_types::{SearchHit, SupersessionStatus};
use areev_context::render::RendererRegistry;
use areev_context::{
    ContextAssembler, DecidePolicy, FormatPolicy, MetadataLevel, OutputFormat, RenderingHints,
};
use areev_core::decide::{Answer, DecideError, DecideRequest, Decision, DecisionBackend, Question};
use areev_core::error::Hash;
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::format::header::MgHeader;
use areev_core::types::GrainType;

// ---------------------------------------------------------------------------
// The scripted backend
// ---------------------------------------------------------------------------

/// Answers by reading markers in each candidate's text: `ANSWER` → level 3,
/// `RELEVANT` → 2, `OFFTOPIC` → 0, otherwise 1; `VERBATIM` → p = 0.9,
/// otherwise 0.1. The intent question gets `intent`.
struct Fake {
    calibrated: bool,
    intent: &'static str,
    fail_intent: bool,
    fail_disclosure: bool,
    /// Answer every `rel_n` with a noul — a malformed answer.
    malformed: bool,
    seen: Mutex<Vec<DecideRequest>>,
}

impl Fake {
    fn new(calibrated: bool) -> Self {
        Fake {
            calibrated,
            intent: "general",
            fail_intent: false,
            fail_disclosure: false,
            malformed: false,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn intent(mut self, i: &'static str) -> Self {
        self.intent = i;
        self
    }
    fn calls(&self) -> Vec<DecideRequest> {
        self.seen.lock().unwrap().clone()
    }
    fn disclosure_calls(&self) -> usize {
        self.calls().iter().filter(|r| !r.questions.contains_key("intent")).count()
    }
    fn intent_calls(&self) -> usize {
        self.calls().iter().filter(|r| r.questions.contains_key("intent")).count()
    }
}

fn score_answer(level: usize) -> Answer {
    let probabilities: BTreeMap<String, f32> =
        (0..4).map(|i| (i.to_string(), if i == level { 1.0 } else { 0.0 })).collect();
    let legend: BTreeMap<String, String> = areev_cal::judge::RELEVANCE_LEVELS
        .iter()
        .enumerate()
        .map(|(i, l)| (i.to_string(), l.to_string()))
        .collect();
    Answer::Score { score: level as f32, probabilities, confidence: 1.0, legend }
}

impl DecisionBackend for Fake {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
        req.validate()?;
        self.seen.lock().unwrap().push(req.clone());
        let mut answers = BTreeMap::new();
        if req.questions.contains_key("intent") {
            if self.fail_intent {
                return Err(DecideError::Deadline("scripted".into()));
            }
            let Question::Choice { criteria, .. } = &req.questions["intent"] else { panic!() };
            let probabilities = criteria
                .keys()
                .map(|k| (k.clone(), if k == self.intent { 1.0 } else { 0.0 }))
                .collect();
            answers.insert(
                "intent".to_string(),
                Answer::Choice { choice: self.intent.to_string(), probabilities, confidence: 1.0 },
            );
        } else {
            if self.fail_disclosure {
                return Err(DecideError::Provider {
                    provider: "fake".into(),
                    status: Some(503),
                    message: "scripted".into(),
                    retryable: true,
                });
            }
            let cands = req.state["candidates"].as_array().unwrap();
            for (n, c) in cands.iter().enumerate() {
                assert_eq!(c["i"], n, "i is the position in this request");
                let text = c["text"].as_str().unwrap();
                let level = if text.contains("ANSWER") {
                    3
                } else if text.contains("RELEVANT") {
                    2
                } else if text.contains("OFFTOPIC") {
                    0
                } else {
                    1
                };
                let rel = if self.malformed { Answer::Noul { p: 0.5 } } else { score_answer(level) };
                answers.insert(format!("rel_{n}"), rel);
                let p = if text.contains("VERBATIM") { 0.9 } else { 0.1 };
                answers.insert(format!("verbatim_{n}"), Answer::Noul { p });
            }
        }
        Ok(Decision {
            answers,
            model: "fake-1".into(),
            provider: "fake".into(),
            calibrated: self.calibrated,
            input_tokens: None,
            output_tokens: None,
            latency_ms: 7,
        })
    }
    fn calibrated(&self) -> bool {
        self.calibrated
    }
    fn describe(&self) -> String {
        "fake:fake-1".into()
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fact(n: u8, subject: &str, object: &str, score: f64, created_at_sec: u32) -> SearchHit {
    let mut fields = HashMap::new();
    fields.insert("subject".to_string(), serde_json::json!(subject));
    fields.insert("relation".to_string(), serde_json::json!("note"));
    fields.insert("object".to_string(), serde_json::json!(object));
    fields.insert("confidence".to_string(), serde_json::json!(0.9));
    let hash = Hash::from_bytes(&[n; 32]);
    SearchHit {
        grain: DeserializedGrain {
            header: MgHeader {
                version: 1,
                flags: 0,
                grain_type: GrainType::Fact.type_byte(),
                ns_hash: 0,
                created_at_sec,
            },
            grain_type: GrainType::Fact,
            fields,
            hash,
        },
        score,
        hash,
        score_breakdown: None,
        explanation: None,
        scope_depth: None,
        source_namespace: None,
        relative_time: None,
        conflict_status: None,
        supersession_status: None,
        superseded_by_hash: None,
        recall_source: None,
    }
}

const DAY: u32 = 86_400;
const T0: u32 = 1_740_000_000;

/// Three facts: an off-topic one ranked FIRST by recall, a relevant one,
/// and the answer (which a summary would lose).
fn three() -> Vec<SearchHit> {
    vec![
        fact(1, "weather", "OFFTOPIC it rained on tuesday", 1.0, T0),
        fact(2, "john", "RELEVANT john joined acme in march", 0.6, T0 + DAY),
        fact(3, "john", "ANSWER VERBATIM john's badge number is 4471-B", 0.3, T0 + 2 * DAY),
    ]
}

fn sml(query: &str) -> FormatPolicy {
    FormatPolicy::new(OutputFormat::Sml)
        .metadata(MetadataLevel::Minimal)
        .no_grain_type_diversity()
        .query_text(query)
}

fn with(fake: &Arc<Fake>) -> ContextAssembler {
    ContextAssembler::new().with_decider(fake.clone())
}

/// Full SML renders carry attributes (`<fact confidence=…`); a Summary is the
/// bare tag (`<fact>`).
fn is_full(text: &str, marker: &str) -> bool {
    text.lines().any(|l| l.contains(marker) && l.starts_with("<fact "))
}
fn is_summary(text: &str, marker: &str) -> bool {
    text.lines().any(|l| l.contains(marker) && l.starts_with("<fact>"))
}

fn tokens(hit: &SearchHit, policy: &FormatPolicy) -> usize {
    RendererRegistry::new().get(GrainType::Fact).unwrap().token_estimate(&hit.grain, policy)
}

// ---------------------------------------------------------------------------
// No decider / failing decider == today
// ---------------------------------------------------------------------------

#[test]
fn no_decider_and_a_failing_decider_render_todays_bytes() {
    let hits = three();
    let formats = [
        OutputFormat::Sml,
        OutputFormat::Markdown,
        OutputFormat::PlainText,
        OutputFormat::Toon,
        OutputFormat::Json,
    ];
    for format in formats {
        for budget in [None, Some(30), Some(80), Some(10_000)] {
            let mut policy = FormatPolicy::new(format.clone()).query_text("john's badge");
            policy.token_budget = budget;
            let today = ContextAssembler::new().format(&hits, &policy);
            assert!(today.decision.is_none());

            let mut failing = Fake::new(true);
            failing.fail_intent = true;
            failing.fail_disclosure = true;
            let failing = Arc::new(failing);
            let shaped = with(&failing).format(&hits, &policy);
            assert_eq!(shaped.text, today.text, "{format:?} {budget:?}");
            assert_eq!(
                (shaped.included_count, shaped.omitted_count, shaped.truncated, shaped.estimated_tokens),
                (today.included_count, today.omitted_count, today.truncated, today.estimated_tokens)
            );
            assert!(shaped.decision.is_none(), "nothing applied → no provenance");
            assert_eq!(failing.calls().len(), 2, "both questions were asked, both failed open");
        }
    }
    // Serialized, the unshaped context has no `decision` key at all.
    let json = serde_json::to_value(ContextAssembler::new().format(&hits, &sml("q"))).unwrap();
    assert!(json.get("decision").is_none(), "{json}");
}

#[test]
fn a_decider_is_not_asked_without_a_query_or_with_one_hit() {
    let fake = Arc::new(Fake::new(true));
    let hits = three();
    let no_query = FormatPolicy::new(OutputFormat::Sml);
    let out = with(&fake).format(&hits, &no_query);
    assert_eq!(out.text, ContextAssembler::new().format(&hits, &no_query).text);
    let one = &hits[..1];
    let out = with(&fake).format(one, &sml("john"));
    assert_eq!(out.text, ContextAssembler::new().format(one, &sml("john")).text);
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[test]
fn a_spent_deadline_asks_nothing_and_renders_today() {
    let fake = Arc::new(Fake::new(true));
    let hits = three();
    let policy = sml("john's badge").decide(DecidePolicy { deadline_ms: Some(0), ..Default::default() });
    let out = with(&fake).format(&hits, &policy);
    assert!(fake.calls().is_empty());
    assert_eq!(out.text, ContextAssembler::new().format(&hits, &policy).text);
    assert!(out.decision.is_none());
}

#[test]
fn a_malformed_answer_falls_back_to_todays_allocation() {
    let mut f = Fake::new(true);
    f.malformed = true;
    let fake = Arc::new(f);
    let hits = three();
    let policy = sml("john's badge").decide(DecidePolicy { intent: false, ..Default::default() });
    let out = with(&fake).format(&hits, &policy);
    assert_eq!(out.text, ContextAssembler::new().format(&hits, &policy).text);
    assert!(out.decision.is_none());
    assert_eq!(fake.disclosure_calls(), 1);
}

// ---------------------------------------------------------------------------
// A2 — calibrated drop / prefer Full; uncalibrated reorder-only
// ---------------------------------------------------------------------------

#[test]
fn calibrated_backend_omits_off_topic_even_without_a_budget() {
    let fake = Arc::new(Fake::new(true));
    let hits = three();
    let policy = sml("what is john's badge number");
    let today = ContextAssembler::new().format(&hits, &policy);
    assert!(today.text.contains("OFFTOPIC"), "positive control: today renders it");

    let out = with(&fake).format(&hits, &policy);
    assert!(!out.text.contains("OFFTOPIC"), "{}", out.text);
    assert!(out.text.contains("RELEVANT") && out.text.contains("ANSWER"));
    assert_eq!((out.included_count, out.omitted_count), (2, 1));
    assert!(out.truncated);
}

#[test]
fn calibrated_verbatim_prefers_full_past_the_70_line_but_not_past_95() {
    // Two answers of equal size T. Budget B with 0.7·B < 2T ≤ 0.95·B: today
    // the second degrades to Summary; preferring Full it fits.
    let a = fact(4, "john", "ANSWER first  detail detail detail detail detail detail detail", 0.9, T0);
    let b = fact(5, "john", "ANSWER VERBATIM second detail detail detail detail detail detail", 0.8, T0 + DAY);
    let policy0 = sml("what is john's badge number");
    let t = tokens(&a, &policy0).max(tokens(&b, &policy0));
    let budget = (2 * t * 100).div_ceil(95) + 1;
    assert!(budget * 70 / 100 < 2 * t, "fixture must bind the 70% line");
    let mut policy = policy0.clone();
    policy.token_budget = Some(budget);
    let hits = vec![a, b];

    let today = ContextAssembler::new().format(&hits, &policy);
    assert!(is_summary(&today.text, "second"), "positive control: {}", today.text);

    let fake = Arc::new(Fake::new(true));
    let out = with(&fake).format(&hits, &policy);
    assert!(is_full(&out.text, "second"), "{}", out.text);
    assert!(is_full(&out.text, "first"), "{}", out.text);

    // The 95% line stays final: a budget that cannot hold both in full keeps
    // the second as a Summary even though a summary would lose it.
    let mut tight = policy.clone();
    tight.token_budget = Some(2 * t);
    let out = with(&fake).format(&hits, &tight);
    assert!(is_summary(&out.text, "second"), "{}", out.text);
}

#[test]
fn uncalibrated_backend_reorders_but_never_omits_or_forces_full() {
    // Room for one Full. Recall ranks the off-topic grain first, so today it
    // gets the Full render and the answer degrades.
    let off = fact(6, "weather", "OFFTOPIC VERBATIM long long long long long long long long", 1.0, T0);
    let ans = fact(7, "john", "ANSWER VERBATIM long long long long long long long long long", 0.2, T0 + DAY);
    let policy0 = sml("what is john's badge number");
    let t = tokens(&off, &policy0).max(tokens(&ans, &policy0));
    let mut policy = policy0.clone();
    policy.token_budget = Some(t * 100 / 70 + 1); // one Full fits under 70%, not two
    let hits = vec![off, ans];

    let today = ContextAssembler::new().format(&hits, &policy);
    assert!(is_full(&today.text, "OFFTOPIC") && is_summary(&today.text, "ANSWER"), "{}", today.text);

    let fake = Arc::new(Fake::new(false));
    let out = with(&fake).format(&hits, &policy);
    // Reordered: the judged answer now takes the Full slot…
    assert!(is_full(&out.text, "ANSWER"), "{}", out.text);
    // …the off-topic grain is NOT omitted (rule 2) and is not forced Full
    // either, though its p_verbatim is 0.9.
    assert!(is_summary(&out.text, "OFFTOPIC"), "{}", out.text);
    assert_eq!(out.omitted_count, 0);
    let d = out.decision.expect("provenance");
    assert!(!d.calibrated);

    // And without a budget nothing is dropped at all.
    let out = with(&fake).format(&hits, &policy0);
    assert!(out.text.contains("OFFTOPIC") && out.omitted_count == 0);
}

#[test]
fn thresholds_are_policy_tunable() {
    // drop_below 0.0 drops nothing; relevance 1/3 (the default level) is
    // dropped at drop_below 0.5.
    let fake = Arc::new(Fake::new(true));
    let hits = three();
    let keep_all = sml("q").decide(DecidePolicy { drop_below: 0.0, ..Default::default() });
    assert_eq!(with(&fake).format(&hits, &keep_all).omitted_count, 0);
    let strict = sml("q").decide(DecidePolicy { drop_below: 0.7, ..Default::default() });
    let out = with(&fake).format(&hits, &strict);
    assert!(out.text.contains("ANSWER") && !out.text.contains("RELEVANT"), "{}", out.text);
    let off = sml("q").decide(DecidePolicy { disclosure: false, intent: false, ..Default::default() });
    let before = fake.calls().len();
    assert_eq!(with(&fake).format(&hits, &off).text, ContextAssembler::new().format(&hits, &off).text);
    assert_eq!(fake.calls().len(), before, "both questions disabled → no call");
}

// ---------------------------------------------------------------------------
// A3 — intent
// ---------------------------------------------------------------------------

fn plain(query: &str) -> FormatPolicy {
    FormatPolicy::new(OutputFormat::PlainText).query_text(query)
}

fn hints(query: &str) -> RenderingHints {
    RenderingHints { query_text: Some(query.to_string()), ..Default::default() }
}

#[test]
fn intent_choice_replaces_the_keyword_list() {
    let hits = three();
    let only_intent = DecidePolicy { disclosure: false, ..Default::default() };
    // No keyword in this query: today it is not a timeline.
    let q = "walk me through john's first week";
    let today = ContextAssembler::new().format_with_hints(&hits, &plain(q), &hints(q));
    assert!(!today.text.contains("Timeline"), "positive control: {}", today.text);

    let fake = Arc::new(Fake::new(true).intent("timeline"));
    let out = with(&fake).format_with_hints(&hits, &plain(q).decide(only_intent.clone()), &hints(q));
    assert!(out.text.starts_with("=== Timeline"), "{}", out.text);
    assert_eq!(out.decision.as_ref().unwrap().intent.as_deref(), Some("timeline"));

    // "when did" IS a keyword; a `general` answer overrides it.
    let q = "when did john join acme";
    let today = ContextAssembler::new().format_with_hints(&hits, &plain(q), &hints(q));
    assert!(today.text.contains("Timeline"), "positive control: {}", today.text);
    let fake = Arc::new(Fake::new(true).intent("general"));
    let out = with(&fake).format_with_hints(&hits, &plain(q).decide(only_intent), &hints(q));
    assert!(!out.text.contains("Timeline"), "{}", out.text);
}

#[test]
fn intent_error_falls_back_to_keywords_and_hint_flags_still_win() {
    let hits = three();
    let only_intent = DecidePolicy { disclosure: false, ..Default::default() };
    let q = "when did john join acme";
    let mut f = Fake::new(true).intent("general");
    f.fail_intent = true;
    let fake = Arc::new(f);
    let out = with(&fake).format_with_hints(&hits, &plain(q).decide(only_intent.clone()), &hints(q));
    assert!(out.text.contains("Timeline"), "keyword fallback: {}", out.text);
    assert!(out.decision.is_none());

    // A parsed time range decides timeline; the question is not even asked.
    let fake = Arc::new(Fake::new(true).intent("general"));
    let flagged = RenderingHints { has_time_range: true, ..hints("anything") };
    let out = with(&fake).format_with_hints(&hits, &plain("anything").decide(only_intent), &flagged);
    assert!(out.text.contains("Timeline"), "{}", out.text);
    assert_eq!(fake.intent_calls(), 0);
}

/// An old value superseded by a new one, both in the result set.
fn chain() -> Vec<SearchHit> {
    let mut old = fact(8, "john", "works at initech", 0.9, T0);
    let mut new = fact(9, "john", "works at acme", 0.8, T0 + 30 * DAY);
    old.supersession_status = Some(SupersessionStatus::Superseded);
    old.superseded_by_hash = Some(new.hash);
    new.supersession_status = Some(SupersessionStatus::Current);
    vec![old, new]
}

#[test]
fn current_state_suppresses_the_old_value_only_when_calibrated() {
    let hits = chain();
    let q = "tell me about john's employer"; // no recency keyword
    let only_intent = DecidePolicy { disclosure: false, ..Default::default() };
    let today = ContextAssembler::new().format_with_hints(&hits, &plain(q), &hints(q));
    assert!(today.text.contains("initech"), "positive control: {}", today.text);

    let cal = Arc::new(Fake::new(true).intent("current_state"));
    let out = with(&cal).format_with_hints(&hits, &plain(q).decide(only_intent.clone()), &hints(q));
    assert!(!out.text.contains("initech") && out.text.contains("acme"), "{}", out.text);

    // Suppressing the old value is an omission: an uncalibrated answer may
    // not decide it (rule 2) — the keyword list does, and it says no.
    let uncal = Arc::new(Fake::new(false).intent("current_state"));
    let out = with(&uncal).format_with_hints(&hits, &plain(q).decide(only_intent), &hints(q));
    assert!(out.text.contains("initech"), "{}", out.text);
    assert_eq!(out.decision.unwrap().intent.as_deref(), Some("current_state"));
}

// ---------------------------------------------------------------------------
// State cap + provenance
// ---------------------------------------------------------------------------

#[test]
fn a_large_candidate_set_splits_under_the_state_cap() {
    // 250 × ~600-char candidates ≈ 38k estimated tokens > the 28k cap.
    let pad = "x".repeat(560);
    let hits: Vec<SearchHit> = (0..250u32)
        .map(|i| {
            let marker = if i % 2 == 0 { "OFFTOPIC" } else { "RELEVANT" };
            let mut h = fact(0, &format!("s{i}"), &format!("{marker} {i} {pad}"), 1.0, T0 + i);
            h.hash = Hash::from_bytes(&[(i % 251) as u8; 32]);
            h.grain.hash = h.hash;
            h
        })
        .collect();
    let fake = Arc::new(Fake::new(true));
    let policy = FormatPolicy::new(OutputFormat::PlainText)
        .query_text("q")
        .decide(DecidePolicy { intent: false, ..Default::default() });
    let out = with(&fake).format(&hits, &policy);

    let calls = fake.calls();
    assert_eq!(calls.len(), 2, "one split, two requests");
    for r in &calls {
        assert!(r.state.to_string().len() / 4 <= areev_cal::judge::MAX_STATE_TOKENS);
    }
    let judged: usize = calls.iter().map(|r| r.state["candidates"].as_array().unwrap().len()).sum();
    assert_eq!(judged, 250, "every candidate judged exactly once");
    // Candidates from BOTH requests were applied: every off-topic grain went.
    assert_eq!((out.included_count, out.omitted_count), (125, 125));
    assert!(!out.text.contains("OFFTOPIC"));
    assert_eq!(out.decision.unwrap().requests, 2);
}

#[test]
fn provenance_names_who_judged() {
    let fake = Arc::new(Fake::new(true).intent("general"));
    let hits = three();
    let out = with(&fake).format_with_hints(&hits, &sml("john's badge"), &hints("john's badge"));
    let d = out.decision.clone().expect("a decision shaped this");
    assert_eq!(d.provider, "fake");
    assert_eq!(d.model, "fake-1");
    assert!(d.calibrated);
    assert_eq!(d.requests, 2, "intent + one disclosure request");
    assert_eq!(d.latency_ms, 14);
    assert_eq!(d.intent.as_deref(), Some("general"));
    let json = serde_json::to_value(&out).unwrap();
    assert_eq!(json["decision"]["provider"], "fake");
    // The disclosure request carries the query and the candidates' text.
    let req = fake.calls().into_iter().find(|r| !r.questions.contains_key("intent")).unwrap();
    assert_eq!(req.state["query"], "john's badge");
    assert_eq!(req.state["candidates"].as_array().unwrap().len(), 3);
    assert_eq!(req.questions.len(), 6);
    assert!(req.questions.contains_key("rel_2") && req.questions.contains_key("verbatim_2"));
}

#[test]
fn retracted_grains_are_never_sent_to_be_judged() {
    let mut hits = three();
    hits[1].grain.fields.insert("verification_status".into(), serde_json::json!("retracted"));
    let fake = Arc::new(Fake::new(true));
    let policy = sml("q").decide(DecidePolicy { intent: false, ..Default::default() });
    let _ = with(&fake).format(&hits, &policy);
    let req = &fake.calls()[0];
    let texts = req.state["candidates"].to_string();
    assert!(!texts.contains("RELEVANT"), "a withheld grain left the process: {texts}");
    assert_eq!(req.state["candidates"].as_array().unwrap().len(), 2);
}

//! CAL lexer — Logos-derived DFA tokenizer for Context Assembly Language.
//!
//! # Security
//!
//! Two mandatory security properties (from the CAL spec) are enforced here
//! before any token is produced:
//!
//! - **S-1**: Bidi override characters (U+202A–U+202E, U+2066–U+2069) are
//!   rejected outright with [`CalError::UnexpectedToken`].  These code-points
//!   can change the visual rendering of source text without affecting the
//!   logical parse tree, enabling "Trojan-Source"-style attacks.
//! - **S-6**: All string input is NFC-normalised before tokenization so that
//!   two visually identical queries always produce the same token stream.
//!
//! # Usage
//!
//! ```
//! # #[cfg(feature = "cal")]
//! # {
//! use areev_cal::lexer::Lexer;
//!
//! let tokens = Lexer::tokenize("RECALL facts WHERE subject = \"john\"").unwrap();
//! # }
//! ```

use logos::Logos;
use unicode_normalization::UnicodeNormalization as _;

use super::errors::{CalError, CalResult, Span};

// ---------------------------------------------------------------------------
// Security helpers (S-1, S-6)
// ---------------------------------------------------------------------------

/// Scan `input` for Unicode bidi-override code points.
///
/// Detected ranges:
/// - U+202A LEFT-TO-RIGHT EMBEDDING
/// - U+202B RIGHT-TO-LEFT EMBEDDING
/// - U+202C POP DIRECTIONAL FORMATTING
/// - U+202D LEFT-TO-RIGHT OVERRIDE
/// - U+202E RIGHT-TO-LEFT OVERRIDE
/// - U+2066 LEFT-TO-RIGHT ISOLATE
/// - U+2067 RIGHT-TO-LEFT ISOLATE
/// - U+2068 FIRST STRONG ISOLATE
/// - U+2069 POP DIRECTIONAL ISOLATE
///
/// Any of these in a CAL query string is a hard error (S-1).
pub fn check_bidi(input: &str) -> CalResult<()> {
    for (byte_offset, ch) in input.char_indices() {
        let cp = ch as u32;
        let is_bidi = matches!(cp, 0x202A..=0x202E | 0x2066..=0x2069);
        if is_bidi {
            return Err(CalError::InvalidUtf8 {
                detail: format!("bidi override U+{:04X} is not permitted (spec §3.1)", cp),
                span: Some(Span::new(
                    byte_offset,
                    byte_offset + ch.len_utf8(),
                    1,
                    byte_offset + 1,
                )),
            });
        }
    }
    Ok(())
}

/// Return the NFC-normalised form of `input` (S-6).
///
/// NFC guarantees that two visually equivalent strings (e.g. a precomposed
/// character vs. a base + combining mark) produce the same token stream.
pub fn nfc_normalize(input: &str) -> String {
    input.nfc().collect()
}

// ---------------------------------------------------------------------------
// Destructive-keyword guard
// ---------------------------------------------------------------------------

/// Return `true` if `word` (compared case-insensitively) is one of the
/// blocked destructive-operation identifiers.
///
/// CAL is a read-oriented language.  Its only destructive statement is the
/// execution-gated `FORGET <hash>`; everything else (bulk erasure, schema
/// changes, key rotation, etc.) is host-level only — the `forget` API or the
/// MCP `areev_forget` tool.
/// Giving these words a hard block at the lexer level ensures that even a
/// future grammar extension cannot accidentally expose a write path.
pub fn is_destructive_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        // FORGET, PURGE, and DROP are first-class Tier-2 CAL statements
        // (authorization-gated, capped by `allow_destructive_ops`), and
        // GRANT/REVOKE are Tier-3 DCL (CAL 1.3) — none blocked here. DELETE
        // stays blocked at the lexer — the deletion verbs are FORGET/PURGE.
        // Credential and key vocabulary stays blocked forever: CAL never
        // touches secrets.
        "DELETE"
            | "ERASE"
            | "DESTROY"
            | "TRUNCATE"
            | "INSERT"
            | "CREATE"
            | "WRITE"
            | "STORE"
            | "KEY"
            | "ENCRYPT"
            | "DECRYPT"
            | "ROTATE"
            | "MASTER"
            | "DEK"
            | "SECRET"
            | "POLICY"
            | "SEAL"
            | "UNSEAL"
            | "TOKEN"
            | "CONSENT"
            | "RESTRICT"
            | "SCHEMA"
            | "PARTITION"
            | "INDEX"
            | "MIGRATION"
    )
}

// ---------------------------------------------------------------------------
// Template section bodies
// ---------------------------------------------------------------------------

/// The body of a template section (OMS CAL §10.6).
///
/// Section bodies hold arbitrary *template text*: prose, apostrophes, angle
/// brackets, an odd number of quotes, words that happen to be CAL keywords.
/// None of that is CAL, so the lexer captures the body as one raw slice
/// instead of tokenizing it — otherwise a template containing `don't` or a
/// lone `"` would fail to lex the whole query.
///
/// `Bare` means the keyword appeared *without* a `{ ... }` body, so it is
/// being used as an ordinary word (a field name, an alias). The parser treats
/// that case as an identifier, which keeps `WHERE header = "x"` working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionBody {
    /// Keyword used as a plain word — no `{` followed.
    Bare,
    /// Raw section text, outer braces removed and layout trimmed.
    Body(String),
}

/// Capture a brace-delimited section body from the raw input.
///
/// Brace matching is mustache-aware: `{{` and `}}` are interpolation
/// delimiters and are stepped over as pairs, so `ELEMENT { {{grain.type}} }`
/// nests correctly. Single braces nest normally, which lets a body contain a
/// JSON object. An unterminated body yields `Bare` so the parser reports the
/// error with a span rather than the lexer failing the whole query.
fn section_body(lex: &mut logos::Lexer<Token>) -> SectionBody {
    let rest = lex.remainder();
    let bytes = rest.as_bytes();

    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'{' {
        return SectionBody::Bare;
    }

    let body_start = i + 1;
    let mut depth = 1usize;
    let mut j = body_start;
    while j < bytes.len() {
        match bytes[j] {
            // `{{` / `}}` are mustache delimiters, not nesting.
            b'{' if bytes.get(j + 1) == Some(&b'{') => j += 2,
            b'}' if bytes.get(j + 1) == Some(&b'}') => j += 2,
            b'{' => {
                depth += 1;
                j += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    if depth != 0 {
        return SectionBody::Bare;
    }

    // ASCII braces cannot occur inside a multi-byte UTF-8 sequence, so these
    // byte offsets are always char boundaries.
    let raw = &rest[body_start..j];
    lex.bump(j + 1);
    SectionBody::Body(trim_section_text(raw))
}

/// Strip the layout that separates a section body from its braces.
///
/// `ELEMENT {\n<fact>…</fact>\n  }` should render `<fact>…</fact>`, not a
/// leading blank line and a trailing indent. Removes at most one newline at
/// each end (with the horizontal whitespace hugging it), so a body that
/// deliberately starts or ends with a blank line keeps it.
fn trim_section_text(raw: &str) -> String {
    let mut s = raw;
    let lead = s.len() - s.trim_start_matches([' ', '\t', '\r']).len();
    if let Some(rest) = s[lead..].strip_prefix('\n') {
        s = rest;
    }
    let end_trimmed = s.trim_end_matches([' ', '\t', '\r']);
    if let Some(rest) = end_trimmed.strip_suffix('\n') {
        s = rest;
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// Token enum
// ---------------------------------------------------------------------------

/// All CAL tokens produced by the DFA lexer.
///
/// Token priority in Logos follows definition order; longer, more specific
/// patterns (keywords, multi-char operators) must appear before catch-all
/// patterns like `Ident`.
#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"([ \t\r\n]+)|(--[^\n]*)")]
pub enum Token {
    // ── Version prefix ────────────────────────────────────────────────────
    /// `CAL` keyword (standalone; parser will look for `/n` after it).
    #[token("CAL", ignore(ascii_case))]
    Cal,

    // ── Tier 0 keywords ───────────────────────────────────────────────────
    #[token("RECALL", ignore(ascii_case))]
    Recall,

    #[token("ASSEMBLE", ignore(ascii_case))]
    Assemble,

    #[token("WHERE", ignore(ascii_case))]
    Where,

    #[token("AND", ignore(ascii_case))]
    And,

    #[token("OR", ignore(ascii_case))]
    Or,

    #[token("NOT", ignore(ascii_case))]
    Not,

    #[token("IN", ignore(ascii_case))]
    In,

    #[token("BETWEEN", ignore(ascii_case))]
    Between,

    #[token("LIMIT", ignore(ascii_case))]
    Limit,

    #[token("OFFSET", ignore(ascii_case))]
    Offset,

    #[token("ORDER", ignore(ascii_case))]
    Order,

    #[token("BY", ignore(ascii_case))]
    By,

    #[token("ASC", ignore(ascii_case))]
    Asc,

    #[token("DESC", ignore(ascii_case))]
    Desc,

    #[token("WITH", ignore(ascii_case))]
    With,

    #[token("EXPLAIN", ignore(ascii_case))]
    Explain,

    #[token("SCOPE", ignore(ascii_case))]
    Scope,

    #[token("UNION", ignore(ascii_case))]
    Union,

    #[token("INTERSECT", ignore(ascii_case))]
    Intersect,

    #[token("EXCEPT", ignore(ascii_case))]
    Except,

    #[token("SELECT", ignore(ascii_case))]
    Select,

    #[token("COUNT", ignore(ascii_case))]
    Count,

    #[token("FIRST", ignore(ascii_case))]
    First,

    #[token("GROUP", ignore(ascii_case))]
    Group,

    #[token("SUBJECTS", ignore(ascii_case))]
    Subjects,

    #[token("OBJECTS", ignore(ascii_case))]
    Objects,

    #[token("HASHES", ignore(ascii_case))]
    Hashes,

    #[token("PROJECT", ignore(ascii_case))]
    Project,

    #[token("INCLUDE", ignore(ascii_case))]
    Include,

    #[token("EXCLUDE", ignore(ascii_case))]
    Exclude,

    #[token("IS", ignore(ascii_case))]
    Is,

    #[token("NULL", ignore(ascii_case))]
    Null,

    #[token("TRUE", ignore(ascii_case))]
    True,

    #[token("FALSE", ignore(ascii_case))]
    False,

    #[token("EXISTS", ignore(ascii_case))]
    Exists,

    #[token("HISTORY", ignore(ascii_case))]
    History,

    #[token("DESCRIBE", ignore(ascii_case))]
    Describe,

    #[token("BATCH", ignore(ascii_case))]
    Batch,

    #[token("COALESCE", ignore(ascii_case))]
    Coalesce,

    #[token("ABOUT", ignore(ascii_case))]
    About,

    #[token("RECENT", ignore(ascii_case))]
    Recent,

    #[token("SINCE", ignore(ascii_case))]
    Since,

    #[token("UNTIL", ignore(ascii_case))]
    Until,

    #[token("LIKE", ignore(ascii_case))]
    Like,

    #[token("MY", ignore(ascii_case))]
    My,

    #[token("CONTRADICTIONS", ignore(ascii_case))]
    Contradictions,

    #[token("AS", ignore(ascii_case))]
    As,

    #[token("FOR", ignore(ascii_case))]
    For,

    #[token("FROM", ignore(ascii_case))]
    From,

    #[token("BUDGET", ignore(ascii_case))]
    Budget,

    #[token("PRIORITY", ignore(ascii_case))]
    Priority,

    /// `LITERAL "…"` — an ASSEMBLE source that is host-supplied text rather
    /// than a query over grains. Lets a compliance-mandated instruction live
    /// in the statement as code instead of as a mutable row.
    #[token("LITERAL", ignore(ascii_case))]
    Literal,

    /// `PIN` — an ASSEMBLE source modifier marking the source non-degradable:
    /// it is disclosed in full or the statement errors, never summarised or
    /// dropped to fit the budget.
    #[token("PIN", ignore(ascii_case))]
    Pin,

    #[token("FORMAT", ignore(ascii_case))]
    Format,

    #[token("LET", ignore(ascii_case))]
    Let,

    #[token("THREAD", ignore(ascii_case))]
    Thread,

    #[token("DIFF", ignore(ascii_case))]
    Diff,

    #[token("STREAM", ignore(ascii_case))]
    Stream,

    #[token("TEMPLATE", ignore(ascii_case))]
    Template,

    #[token("DEFINE", ignore(ascii_case))]
    Define,

    #[token("DROP", ignore(ascii_case))]
    Drop,

    #[token("QUERY", ignore(ascii_case))]
    Query,

    #[token("RUN", ignore(ascii_case))]
    Run,

    #[token("EXTENDS", ignore(ascii_case))]
    Extends,

    // Template sections (OMS CAL §10.6). Each carries its raw body — see
    // [`SectionBody`] for why the body is not tokenized.
    #[token("HEADER", section_body, ignore(ascii_case))]
    Header(SectionBody),

    #[token("ELEMENT", section_body, ignore(ascii_case))]
    Element(SectionBody),

    #[token("ELEMENT_SUMMARY", section_body, ignore(ascii_case))]
    ElementSummary(SectionBody),

    #[token("ELEMENT_OMIT", section_body, ignore(ascii_case))]
    ElementOmit(SectionBody),

    #[token("SOURCE_BREAK", section_body, ignore(ascii_case))]
    SourceBreak(SectionBody),

    #[token("FOOTER", section_body, ignore(ascii_case))]
    Footer(SectionBody),

    #[token("OF", ignore(ascii_case))]
    Of,

    // ── Tier 1 keywords (write statements) ───────────────────────────────
    #[token("ADD", ignore(ascii_case))]
    Add,

    #[token("ACCUMULATE", ignore(ascii_case))]
    Accumulate,

    #[token("SUPERSEDE", ignore(ascii_case))]
    Supersede,

    #[token("REVERT", ignore(ascii_case))]
    Revert,

    // ── Tier 2 keywords (destructive statements) ─────────────────────────
    /// `REMEMBER "<content>"` (CAL 1.3) — capture an Event, the
    /// highest-frequency onboarding verb.
    #[token("REMEMBER", ignore(ascii_case))]
    Remember,

    /// `ENTITY "<s>" RELATION "<r>" AT <ms>` (CAL 1.3) — the as-of read.
    #[token("ENTITY", ignore(ascii_case))]
    Entity,

    /// `MERGE "<s>" RELATION "<r>" TO "<o>"` (CAL 1.3) — close a fork.
    #[token("MERGE", ignore(ascii_case))]
    Merge,

    /// `RELATED "<start>" VIA "<relations>"` (CAL 1.3) — the graph walk.
    #[token("RELATED", ignore(ascii_case))]
    Related,

    /// `NOVELTY "<text>"` (CAL 1.3) — nearest existing grains.
    #[token("NOVELTY", ignore(ascii_case))]
    Novelty,

    /// `RUNS TOUCHING <hash>` (CAL 1.3) — the reverse run↔memory join.
    #[token("RUNS", ignore(ascii_case))]
    Runs,

    /// `DERIVED FROM <hash>` (CAL 1.3) — reverse provenance.
    #[token("DERIVED", ignore(ascii_case))]
    Derived,

    /// Governance (CAL 1.3 §8.16): `APPROVE <hash> BECAUSE "<why>"`.
    #[token("APPROVE", ignore(ascii_case))]
    Approve,

    /// Governance: `REJECT <hash> BECAUSE "<why>"`.
    #[token("REJECT", ignore(ascii_case))]
    Reject,

    /// Governance: `APPLY <hash> BECAUSE "<why>"`.
    #[token("APPLY", ignore(ascii_case))]
    Apply,

    /// Governance: `ROLLBACK <hash> BECAUSE "<why>"`.
    #[token("ROLLBACK", ignore(ascii_case))]
    Rollback,

    /// Tier-3 DCL (CAL 1.3): `GRANT <verbs> ON <ns> TO "<principal>"`.
    #[token("GRANT", ignore(ascii_case))]
    Grant,

    /// Tier-3 DCL (CAL 1.3): `REVOKE <verbs> ON <ns> FROM "<principal>"`.
    #[token("REVOKE", ignore(ascii_case))]
    Revoke,

    /// `SHOW GRANTS [FOR "<principal>"]` (CAL 1.3).
    #[token("SHOW", ignore(ascii_case))]
    Show,

    /// `TO` — the GRANT recipient marker.
    #[token("TO", ignore(ascii_case))]
    To,

    #[token("FORGET", ignore(ascii_case))]
    Forget,

    #[token("PURGE", ignore(ascii_case))]
    Purge,

    /// `REPORT SUBJECT` — the read-only DSAR selection (OMS 1.6 draft).
    #[token("REPORT", ignore(ascii_case))]
    Report,

    #[token("SET", ignore(ascii_case))]
    Set,

    #[token("REASON", ignore(ascii_case))]
    Reason,

    #[token("BECAUSE", ignore(ascii_case))]
    Because,

    // ── Relation-category keywords ────────────────────────────────────────
    #[token("PREFERENCE", ignore(ascii_case))]
    Preference,

    #[token("KNOWLEDGE", ignore(ascii_case))]
    Knowledge,

    #[token("PERMISSION", ignore(ascii_case))]
    Permission,

    #[token("INTERACTION", ignore(ascii_case))]
    Interaction,

    #[token("AGENCY", ignore(ascii_case))]
    Agency,

    #[token("LIFECYCLE", ignore(ascii_case))]
    Lifecycle,

    #[token("OBSERVATION", ignore(ascii_case))]
    Observation,

    // ── Format type keywords ──────────────────────────────────────────────
    #[token("MARKDOWN", ignore(ascii_case))]
    Markdown,

    #[token("JSON", ignore(ascii_case))]
    Json,

    #[token("YAML", ignore(ascii_case))]
    Yaml,

    #[token("TEXT", ignore(ascii_case))]
    Text,

    #[token("SML", ignore(ascii_case))]
    Sml,

    #[token("TOON", ignore(ascii_case))]
    Toon,

    #[token("TRIPLES", ignore(ascii_case))]
    Triples,

    // ── Format preset keywords ────────────────────────────────────────────
    #[token("STRUCTURED", ignore(ascii_case))]
    Structured,

    #[token("READABLE", ignore(ascii_case))]
    Readable,

    #[token("COMPACT", ignore(ascii_case))]
    Compact,

    #[token("DATA", ignore(ascii_case))]
    Data,

    // ── STREAM option keywords ────────────────────────────────────────────
    #[token("PROGRESS", ignore(ascii_case))]
    Progress,

    #[token("CHUNKS", ignore(ascii_case))]
    Chunks,

    #[token("ALL", ignore(ascii_case))]
    All,

    #[token("CHUNK_SIZE", ignore(ascii_case))]
    ChunkSize,

    // ── WITH option keywords ──────────────────────────────────────────────
    #[token("SUPERSEDED", ignore(ascii_case))]
    Superseded,

    #[token("SCORE_BREAKDOWN", ignore(ascii_case))]
    ScoreBreakdown,

    #[token("EXPLANATION", ignore(ascii_case))]
    Explanation,

    #[token("PROVENANCE", ignore(ascii_case))]
    Provenance,

    #[token("CONTRADICTION_DETECTION", ignore(ascii_case))]
    ContradictionDetection,

    #[token("DIVERSITY", ignore(ascii_case))]
    Diversity,

    #[token("DEDUP", ignore(ascii_case))]
    Dedup,

    // OMS §4 WITH options not previously tokenized — added so the spec
    // syntax parses without falling through to UnknownExtensionOption.
    #[token("PROGRESSIVE_DISCLOSURE", ignore(ascii_case))]
    ProgressiveDisclosure,

    #[token("CONSISTENCY", ignore(ascii_case))]
    Consistency,

    #[token("LOCALE", ignore(ascii_case))]
    Locale,

    #[token("CACHE", ignore(ascii_case))]
    Cache,

    #[token("TTL", ignore(ascii_case))]
    Ttl,

    // ── WITH option keywords (recall feature flags) ─────────────────────
    #[token("RERANK", ignore(ascii_case))]
    Rerank,

    #[token("LLM_RERANK", ignore(ascii_case))]
    LlmRerank,

    #[token("QUERY_EXPANSION", ignore(ascii_case))]
    QueryExpansion,

    #[token("QUERY_DECOMPOSE", ignore(ascii_case))]
    QueryDecompose,

    #[token("HYDE", ignore(ascii_case))]
    Hyde,

    #[token("CONFLICT_RESOLUTION", ignore(ascii_case))]
    ConflictResolution,

    #[token("INCLUDE_SOURCES", ignore(ascii_case))]
    IncludeSources,

    #[token("ANNOTATE_RELATIVE_TIME", ignore(ascii_case))]
    AnnotateRelativeTime,

    #[token("RECENCY_WEIGHT", ignore(ascii_case))]
    RecencyWeight,

    #[token("MIN_SCORE", ignore(ascii_case))]
    MinScore,

    #[token("MULTI_HOP", ignore(ascii_case))]
    MultiHop,

    #[token("SESSION_AFFINITY", ignore(ascii_case))]
    SessionAffinity,

    #[token("SUBJECT_AFFINITY", ignore(ascii_case))]
    SubjectAffinity,

    #[token("SESSION_COVERAGE", ignore(ascii_case))]
    SessionCoverage,

    #[token("MAX_NAMESPACES", ignore(ascii_case))]
    MaxNamespaces,

    #[token("EXHAUSTIVE", ignore(ascii_case))]
    Exhaustive,

    #[token("SESSION_CENSUS", ignore(ascii_case))]
    SessionCensus,

    #[token("AGGREGATION_INTENT", ignore(ascii_case))]
    AggregationIntent,

    #[token("PREFERENCE_ENRICHMENT", ignore(ascii_case))]
    PreferenceEnrichment,

    // ── ADD WITH option keywords ────────────────────────────────────────
    #[token("EXTRACT_EVENT_DATE", ignore(ascii_case))]
    ExtractEventDate,

    #[token("AUTO_RELATE", ignore(ascii_case))]
    AutoRelate,

    #[token("EXTRACT_MEMORIES", ignore(ascii_case))]
    ExtractMemories,

    #[token("SYNC", ignore(ascii_case))]
    SyncOption,

    #[token("VARS", ignore(ascii_case))]
    Vars,

    // ── Workflow graph keywords ────────────────────────────────────────────
    #[token("ON", ignore(ascii_case))]
    On,

    #[token("WHEN", ignore(ascii_case))]
    When,

    #[token("BIND", ignore(ascii_case))]
    Bind,

    // ── Operators (multi-char first to avoid ambiguity) ───────────────────
    #[token("->")]
    Arrow,

    #[token("!=")]
    NotEq,

    #[token(">=")]
    Gte,

    #[token("<=")]
    Lte,

    #[token(">")]
    Gt,

    #[token("<")]
    Lt,

    #[token("=")]
    Eq,

    #[token("*")]
    Asterisk,

    // ── Punctuation ───────────────────────────────────────────────────────
    #[token("(")]
    LParen,

    #[token(")")]
    RParen,

    #[token("[")]
    LBracket,

    #[token("]")]
    RBracket,

    #[token("{")]
    LBrace,

    #[token("}")]
    RBrace,

    #[token(",")]
    Comma,

    #[token(";")]
    Semicolon,

    #[token("|")]
    Pipe,

    #[token("/")]
    Slash,

    #[token(":")]
    Colon,

    #[token(".")]
    Dot,

    #[token("$")]
    Dollar,

    #[token("#")]
    Hash,

    // ── Literals — order matters: more specific patterns must come first ──
    /// `sha256:` followed by 8–64 lowercase hex characters.
    ///
    /// The `sha256:` prefix is consumed but not included in the payload; only
    /// the hex digest is stored. OMS-1.4 §3.4 defines the literal as
    /// `hash_literal = "sha256:" , hex_char{8,64}` — Bug 17 of the audit tightened
    /// the minimum from 4 to 8 to match spec. Short hashes (< 64 chars) are
    /// accepted at lex time; the executor resolves them against the store.
    #[regex("(?i:sha256:)[0-9a-fA-F]{8,64}", |lex| {
        // Find the colon (the prefix is 6 chars "sha256")
        let s = lex.slice();
        let colon_pos = s.find(':').unwrap_or(6);
        s[colon_pos + 1..].to_ascii_lowercase()
    })]
    HashLiteral(String),

    /// `$identifier` — a named parameter reference.
    ///
    /// The leading `$` is consumed; only the identifier name is stored.
    #[regex(r"\$[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice()[1..].to_string())]
    Parameter(String),

    /// A double-quoted string literal with `\"` and `\\` escape sequences.
    /// Single left-to-right pass to avoid the double-decode pitfall of
    /// chained `String::replace` (e.g. `"\\\\n"` must remain a literal `\n`
    /// pair, not a newline).
    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let slice = lex.slice();
        let inner = &slice[1..slice.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('"')  => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n')  => out.push('\n'),
                    Some('t')  => out.push('\t'),
                    Some('r')  => out.push('\r'),
                    Some(other) => { out.push('\\'); out.push(other); }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    })]
    StringLiteral(String),

    /// An unterminated string literal (opening `"` with no matching `"`).
    ///
    /// This token exists so that the lexer can detect the error rather than
    /// silently emitting a logos error.
    #[regex(r#""([^"\\]|\\.)*"#)]
    UnterminatedString,

    /// A numeric literal: optional `-`, digits, optional decimal part.
    #[regex(r"-?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?", |lex| lex.slice().parse::<f64>().ok())]
    NumberLiteral(f64),

    // ── Identifiers ───────────────────────────────────────────────────────
    /// Any identifier that did not match a keyword.
    ///
    /// The parser uses [`is_destructive_keyword`] on identifiers to give
    /// precise error messages when a user accidentally writes a write-path
    /// command in a read-only CAL context.
    #[regex("[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    Ident(String),
}

impl Token {
    /// Return a human-readable description of the token (used in error
    /// messages).
    pub fn description(&self) -> String {
        match self {
            Token::Cal => "CAL".into(),
            Token::Recall => "RECALL".into(),
            Token::Assemble => "ASSEMBLE".into(),
            Token::Literal => "LITERAL".into(),
            Token::Pin => "PIN".into(),
            Token::Where => "WHERE".into(),
            Token::And => "AND".into(),
            Token::Or => "OR".into(),
            Token::Not => "NOT".into(),
            Token::In => "IN".into(),
            Token::Between => "BETWEEN".into(),
            Token::Limit => "LIMIT".into(),
            Token::Offset => "OFFSET".into(),
            Token::Order => "ORDER".into(),
            Token::By => "BY".into(),
            Token::Asc => "ASC".into(),
            Token::Desc => "DESC".into(),
            Token::With => "WITH".into(),
            Token::Explain => "EXPLAIN".into(),
            Token::Scope => "SCOPE".into(),
            Token::Union => "UNION".into(),
            Token::Intersect => "INTERSECT".into(),
            Token::Except => "EXCEPT".into(),
            Token::Select => "SELECT".into(),
            Token::Count => "COUNT".into(),
            Token::First => "FIRST".into(),
            Token::Group => "GROUP".into(),
            Token::Subjects => "SUBJECTS".into(),
            Token::Objects => "OBJECTS".into(),
            Token::Hashes => "HASHES".into(),
            Token::Project => "PROJECT".into(),
            Token::Include => "INCLUDE".into(),
            Token::Exclude => "EXCLUDE".into(),
            Token::Is => "IS".into(),
            Token::Null => "NULL".into(),
            Token::True => "TRUE".into(),
            Token::False => "FALSE".into(),
            Token::Exists => "EXISTS".into(),
            Token::History => "HISTORY".into(),
            Token::Describe => "DESCRIBE".into(),
            Token::Batch => "BATCH".into(),
            Token::Coalesce => "COALESCE".into(),
            Token::About => "ABOUT".into(),
            Token::Recent => "RECENT".into(),
            Token::Since => "SINCE".into(),
            Token::Until => "UNTIL".into(),
            Token::Like => "LIKE".into(),
            Token::My => "MY".into(),
            Token::Contradictions => "CONTRADICTIONS".into(),
            Token::As => "AS".into(),
            Token::For => "FOR".into(),
            Token::From => "FROM".into(),
            Token::Budget => "BUDGET".into(),
            Token::Priority => "PRIORITY".into(),
            Token::Format => "FORMAT".into(),
            Token::Let => "LET".into(),
            Token::Thread => "THREAD".into(),
            Token::Diff => "DIFF".into(),
            Token::Stream => "STREAM".into(),
            Token::Template => "TEMPLATE".into(),
            Token::Define => "DEFINE".into(),
            Token::Drop => "DROP".into(),
            Token::Query => "QUERY".into(),
            Token::Run => "RUN".into(),
            Token::Extends => "EXTENDS".into(),
            Token::Header(_) => "HEADER".into(),
            Token::Element(_) => "ELEMENT".into(),
            Token::ElementSummary(_) => "ELEMENT_SUMMARY".into(),
            Token::ElementOmit(_) => "ELEMENT_OMIT".into(),
            Token::SourceBreak(_) => "SOURCE_BREAK".into(),
            Token::Footer(_) => "FOOTER".into(),
            Token::Of => "OF".into(),
            Token::On => "ON".into(),
            Token::When => "WHEN".into(),
            Token::Bind => "BIND".into(),
            Token::Arrow => "->".into(),
            Token::Asterisk => "*".into(),
            Token::Add => "ADD".into(),
            Token::Accumulate => "ACCUMULATE".into(),
            Token::Supersede => "SUPERSEDE".into(),
            Token::Revert => "REVERT".into(),
            Token::Grant => "GRANT".into(),
            Token::Revoke => "REVOKE".into(),
            Token::Show => "SHOW".into(),
            Token::To => "TO".into(),
            Token::Remember => "REMEMBER".into(),
            Token::Entity => "ENTITY".into(),
            Token::Merge => "MERGE".into(),
            Token::Related => "RELATED".into(),
            Token::Novelty => "NOVELTY".into(),
            Token::Runs => "RUNS".into(),
            Token::Derived => "DERIVED".into(),
            Token::Approve => "APPROVE".into(),
            Token::Reject => "REJECT".into(),
            Token::Apply => "APPLY".into(),
            Token::Rollback => "ROLLBACK".into(),
            Token::Forget => "FORGET".into(),
            Token::Purge => "PURGE".into(),
            Token::Report => "REPORT".into(),
            Token::Set => "SET".into(),
            Token::Reason => "REASON".into(),
            Token::Because => "BECAUSE".into(),
            Token::Preference => "PREFERENCE".into(),
            Token::Knowledge => "KNOWLEDGE".into(),
            Token::Permission => "PERMISSION".into(),
            Token::Interaction => "INTERACTION".into(),
            Token::Agency => "AGENCY".into(),
            Token::Lifecycle => "LIFECYCLE".into(),
            Token::Observation => "OBSERVATION".into(),
            Token::Markdown => "MARKDOWN".into(),
            Token::Json => "JSON".into(),
            Token::Yaml => "YAML".into(),
            Token::Text => "TEXT".into(),
            Token::Sml => "SML".into(),
            Token::Toon => "TOON".into(),
            Token::Triples => "TRIPLES".into(),
            Token::Structured => "STRUCTURED".into(),
            Token::Readable => "READABLE".into(),
            Token::Compact => "COMPACT".into(),
            Token::Data => "DATA".into(),
            Token::Progress => "PROGRESS".into(),
            Token::Chunks => "CHUNKS".into(),
            Token::All => "ALL".into(),
            Token::ChunkSize => "CHUNK_SIZE".into(),
            Token::Superseded => "SUPERSEDED".into(),
            Token::ScoreBreakdown => "SCORE_BREAKDOWN".into(),
            Token::Explanation => "EXPLANATION".into(),
            Token::Provenance => "PROVENANCE".into(),
            Token::ContradictionDetection => "CONTRADICTION_DETECTION".into(),
            Token::Diversity => "DIVERSITY".into(),
            Token::Dedup => "DEDUP".into(),
            Token::Rerank => "RERANK".into(),
            Token::LlmRerank => "LLM_RERANK".into(),
            Token::QueryExpansion => "QUERY_EXPANSION".into(),
            Token::QueryDecompose => "QUERY_DECOMPOSE".into(),
            Token::Hyde => "HYDE".into(),
            Token::ConflictResolution => "CONFLICT_RESOLUTION".into(),
            Token::IncludeSources => "INCLUDE_SOURCES".into(),
            Token::AnnotateRelativeTime => "ANNOTATE_RELATIVE_TIME".into(),
            Token::RecencyWeight => "RECENCY_WEIGHT".into(),
            Token::MinScore => "MIN_SCORE".into(),
            Token::MultiHop => "MULTI_HOP".into(),
            Token::SessionAffinity => "SESSION_AFFINITY".into(),
            Token::SubjectAffinity => "SUBJECT_AFFINITY".into(),
            Token::SessionCoverage => "SESSION_COVERAGE".into(),
            Token::MaxNamespaces => "MAX_NAMESPACES".into(),
            Token::Exhaustive => "EXHAUSTIVE".into(),
            Token::SessionCensus => "SESSION_CENSUS".into(),
            Token::AggregationIntent => "AGGREGATION_INTENT".into(),
            Token::PreferenceEnrichment => "PREFERENCE_ENRICHMENT".into(),
            Token::ExtractEventDate => "EXTRACT_EVENT_DATE".into(),
            Token::AutoRelate => "AUTO_RELATE".into(),
            Token::ExtractMemories => "EXTRACT_MEMORIES".into(),
            Token::SyncOption => "SYNC".into(),
            Token::Vars => "VARS".into(),
            Token::NotEq => "!=".into(),
            Token::Gte => ">=".into(),
            Token::Lte => "<=".into(),
            Token::Gt => ">".into(),
            Token::Lt => "<".into(),
            Token::Eq => "=".into(),
            Token::LParen => "(".into(),
            Token::RParen => ")".into(),
            Token::LBracket => "[".into(),
            Token::RBracket => "]".into(),
            Token::LBrace => "{".into(),
            Token::RBrace => "}".into(),
            Token::Comma => ",".into(),
            Token::Semicolon => ";".into(),
            Token::Pipe => "|".into(),
            Token::Slash => "/".into(),
            Token::Colon => ":".into(),
            Token::Dot => ".".into(),
            Token::Dollar => "$".into(),
            Token::Hash => "#".into(),
            Token::HashLiteral(h) => format!("sha256:{}", h),
            Token::Parameter(n) => format!("${}", n),
            Token::StringLiteral(s) => format!("\"{}\"", s),
            Token::UnterminatedString => "unterminated string".into(),
            Token::NumberLiteral(n) => n.to_string(),
            Token::Ident(i) => i.clone(),
            Token::ProgressiveDisclosure => "PROGRESSIVE_DISCLOSURE".into(),
            Token::Consistency => "CONSISTENCY".into(),
            Token::Locale => "LOCALE".into(),
            Token::Cache => "CACHE".into(),
            Token::Ttl => "TTL".into(),
        }
    }

    /// Return `true` if this token is a keyword that starts a statement.
    pub fn is_statement_starter(&self) -> bool {
        matches!(
            self,
            Token::Recall
                | Token::Assemble
                | Token::Exists
                | Token::History
                | Token::Explain
                | Token::Describe
                | Token::Batch
                | Token::Coalesce
                | Token::Add
                | Token::Accumulate
                | Token::Supersede
                | Token::Revert
                | Token::Forget
                | Token::Purge
                | Token::Report
                | Token::Drop
                | Token::Run
        )
    }
}

// ---------------------------------------------------------------------------
// SpannedToken
// ---------------------------------------------------------------------------

/// A token together with its source location and original text.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    /// The lexed token value.
    pub token: Token,
    /// Byte-offset span in the (NFC-normalised) source string.
    pub span: Span,
    /// The literal text slice that produced this token.
    pub text: String,
}

impl SpannedToken {
    /// Create a new [`SpannedToken`].
    pub fn new(token: Token, span: Span, text: impl Into<String>) -> Self {
        Self {
            token,
            span,
            text: text.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

/// CAL lexer — wraps the Logos DFA and owns the normalised source string.
///
/// Construct with [`Lexer::new`] (validates bidi + normalises NFC), then
/// call [`Lexer::tokenize`] for a one-shot token list.
pub struct Lexer {
    /// NFC-normalised source (owned so we can hand out `&str` slices).
    source: String,
}

impl Lexer {
    /// Create a new [`Lexer`] for the given raw input.
    ///
    /// # Errors
    ///
    /// Returns [`CalError::UnexpectedToken`] if the input contains bidi
    /// override characters (S-1).
    pub fn new(input: &str) -> CalResult<Self> {
        // S-1: reject bidi override characters before NFC normalisation.
        check_bidi(input)?;
        // S-6: NFC normalise.
        let source = nfc_normalize(input);
        Ok(Self { source })
    }

    /// Tokenize the stored source and return a flat `Vec<SpannedToken>`.
    ///
    /// This is the primary entry point for the parser.
    ///
    /// # Errors
    ///
    /// - [`CalError::UnterminatedString`] for an unclosed `"` literal.
    /// - [`CalError::UnexpectedToken`] for any unrecognised character.
    pub fn run(&self) -> CalResult<Vec<SpannedToken>> {
        Self::tokenize_str(&self.source)
    }

    /// One-shot helper — applies bidi check, NFC normalisation, and
    /// tokenization in a single call.
    pub fn tokenize(input: &str) -> CalResult<Vec<SpannedToken>> {
        let lexer = Self::new(input)?;
        lexer.run()
    }

    /// Return a reference to the normalised source string.
    pub fn source(&self) -> &str {
        &self.source
    }

    // -- private -------------------------------------------------------

    fn tokenize_str(source: &str) -> CalResult<Vec<SpannedToken>> {
        let mut tokens = Vec::new();
        let mut line: usize = 1;
        let mut line_start: usize = 0;

        let mut logos_lex = Token::lexer(source);

        while let Some(result) = logos_lex.next() {
            let logos_span = logos_lex.span();
            let text = logos_lex.slice().to_string();

            // I-8 fix: precisely track line_start by recording the byte
            // position *after* each newline, not the token start.
            // Capture the slice base once — line_start mutates inside the loop
            // and using the mutated value as the offset base double-counts on
            // the second+ newline (panics with subtract overflow on line 970).
            let slice_base = line_start;
            for (idx, byte) in source[slice_base..logos_span.start].bytes().enumerate() {
                if byte == b'\n' {
                    line += 1;
                    // line_start is the absolute byte offset of the first
                    // character on the new line (= position after the '\n').
                    line_start = slice_base + idx + 1;
                }
            }
            let col = logos_span.start - line_start + 1;
            let span = Span::new(logos_span.start, logos_span.end, line, col);

            match result {
                Ok(token) => {
                    // Check for unterminated string before emitting.
                    if token == Token::UnterminatedString {
                        return Err(CalError::UnterminatedString { span: Some(span) });
                    }
                    tokens.push(SpannedToken::new(token, span, text));
                }
                Err(()) => {
                    // Logos Error variant — unrecognised character.
                    return Err(CalError::UnexpectedToken {
                        expected: "a valid CAL token".into(),
                        found: format!("{:?}", text),
                        span: Some(span),
                        suggestion: None,
                    });
                }
            }
        }

        Ok(tokens)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(input: &str) -> Vec<Token> {
        Lexer::tokenize(input)
            .unwrap()
            .into_iter()
            .map(|st| st.token)
            .collect()
    }

    // ── 1. Basic keyword recognition ──────────────────────────────────────

    #[test]
    fn test_recall_keyword() {
        let tokens = tok("RECALL");
        assert_eq!(tokens, vec![Token::Recall]);
    }

    #[test]
    fn test_where_and_or_keywords() {
        let tokens = tok("WHERE AND OR NOT");
        assert_eq!(
            tokens,
            vec![Token::Where, Token::And, Token::Or, Token::Not]
        );
    }

    #[test]
    fn test_pipeline_keywords() {
        let tokens = tok("SELECT ORDER BY ASC DESC LIMIT OFFSET COUNT FIRST");
        assert_eq!(
            tokens,
            vec![
                Token::Select,
                Token::Order,
                Token::By,
                Token::Asc,
                Token::Desc,
                Token::Limit,
                Token::Offset,
                Token::Count,
                Token::First,
            ]
        );
    }

    // ── 2. Case insensitivity ─────────────────────────────────────────────

    #[test]
    fn test_case_insensitive_keywords() {
        let tokens = tok("recall Recall RECALL rEcAlL");
        assert_eq!(
            tokens,
            vec![Token::Recall, Token::Recall, Token::Recall, Token::Recall]
        );
    }

    #[test]
    fn test_case_insensitive_where() {
        let tokens = tok("where WHERE Where");
        assert_eq!(tokens, vec![Token::Where, Token::Where, Token::Where]);
    }

    // ── 3. String literals ────────────────────────────────────────────────

    #[test]
    fn test_string_literal_basic() {
        let tokens = tok(r#""hello world""#);
        assert_eq!(tokens, vec![Token::StringLiteral("hello world".into())]);
    }

    #[test]
    fn test_string_literal_with_escape() {
        let tokens = tok(r#""say \"hi\"""#);
        assert_eq!(tokens, vec![Token::StringLiteral("say \"hi\"".into())]);
    }

    #[test]
    fn test_string_literal_with_backslash_escape() {
        let tokens = tok(r#""path\\file""#);
        assert_eq!(tokens, vec![Token::StringLiteral("path\\file".into())]);
    }

    // ── 4. Number literals ────────────────────────────────────────────────

    #[test]
    fn test_integer_literal() {
        let tokens = tok("42");
        assert_eq!(tokens, vec![Token::NumberLiteral(42.0)]);
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_float_literal() {
        let tokens = tok("3.14");
        assert_eq!(tokens, vec![Token::NumberLiteral(3.14)]);
    }

    #[test]
    fn test_negative_number() {
        let tokens = tok("-5");
        assert_eq!(tokens, vec![Token::NumberLiteral(-5.0)]);
    }

    // ── 5. Hash literals ──────────────────────────────────────────────────

    #[test]
    fn test_hash_literal_lowercase() {
        let tokens = tok("sha256:abc123def456");
        assert_eq!(tokens, vec![Token::HashLiteral("abc123def456".into())]);
    }

    #[test]
    fn test_hash_literal_uppercase_prefix() {
        let tokens = tok("SHA256:ABC123DEF456");
        assert_eq!(tokens, vec![Token::HashLiteral("abc123def456".into())]);
    }

    #[test]
    fn test_hash_literal_full_64_chars() {
        let digest = "a".repeat(64);
        let input = format!("sha256:{}", digest);
        let tokens = tok(&input);
        assert_eq!(tokens, vec![Token::HashLiteral(digest)]);
    }

    // ── 6. Parameters ─────────────────────────────────────────────────────

    #[test]
    fn test_parameter_basic() {
        let tokens = tok("$name");
        assert_eq!(tokens, vec![Token::Parameter("name".into())]);
    }

    #[test]
    fn test_parameter_with_underscore() {
        let tokens = tok("$user_id");
        assert_eq!(tokens, vec![Token::Parameter("user_id".into())]);
    }

    // ── 7. Operators ──────────────────────────────────────────────────────

    #[test]
    fn test_comparison_operators() {
        let tokens = tok("= != >= <= > <");
        assert_eq!(
            tokens,
            vec![
                Token::Eq,
                Token::NotEq,
                Token::Gte,
                Token::Lte,
                Token::Gt,
                Token::Lt,
            ]
        );
    }

    // ── 8. Comments ───────────────────────────────────────────────────────

    #[test]
    fn test_line_comment_ignored() {
        // NOTE: an earlier revision had input "beliefs" vs expectation "facts" —
        // a belief→fact rename artifact, latent there because the `cal`
        // feature was off in the default CI. Made self-consistent here.
        let tokens = tok("RECALL -- this is a comment\nfacts");
        assert_eq!(tokens, vec![Token::Recall, Token::Ident("facts".into())]);
    }

    #[test]
    fn test_comment_at_end_of_input() {
        let tokens = tok("RECALL -- end comment");
        assert_eq!(tokens, vec![Token::Recall]);
    }

    // ── 9. Bidi character rejection (S-1) ─────────────────────────────────

    #[test]
    fn test_bidi_ltr_embedding_rejected() {
        let input = "RECALL \u{202A}facts";
        let err = Lexer::tokenize(input).unwrap_err();
        assert!(err.to_string().contains("bidi override"));
    }

    #[test]
    fn test_bidi_rtl_override_rejected() {
        let input = "RECALL \u{202E}facts";
        let err = Lexer::tokenize(input).unwrap_err();
        assert!(err.to_string().contains("bidi override"));
    }

    #[test]
    fn test_bidi_isolate_rejected() {
        let input = "RECALL \u{2066}facts";
        let err = Lexer::tokenize(input).unwrap_err();
        assert!(err.to_string().contains("bidi override"));
    }

    #[test]
    fn test_bidi_all_codepoints_rejected_s1() {
        // All 9 bidi override/isolate codepoints (U+202A-U+202E, U+2066-U+2069)
        // must be rejected per S-1.
        let bidi_chars = [
            '\u{202A}', // LEFT-TO-RIGHT EMBEDDING
            '\u{202B}', // RIGHT-TO-LEFT EMBEDDING
            '\u{202C}', // POP DIRECTIONAL FORMATTING
            '\u{202D}', // LEFT-TO-RIGHT OVERRIDE
            '\u{202E}', // RIGHT-TO-LEFT OVERRIDE
            '\u{2066}', // LEFT-TO-RIGHT ISOLATE
            '\u{2067}', // RIGHT-TO-LEFT ISOLATE
            '\u{2068}', // FIRST STRONG ISOLATE
            '\u{2069}', // POP DIRECTIONAL ISOLATE
        ];
        for ch in &bidi_chars {
            let input = format!("RECALL {}facts", ch);
            let result = Lexer::tokenize(&input);
            assert!(
                result.is_err(),
                "Bidi character U+{:04X} should be rejected (S-1)",
                *ch as u32
            );
            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("bidi override"),
                "Error for U+{:04X} should mention bidi override, got: {}",
                *ch as u32,
                err
            );
        }
    }

    #[test]
    fn test_bidi_inside_string_literal_rejected_s1() {
        // Bidi chars must be rejected even when embedded inside a string literal.
        let input = format!("RECALL facts WHERE subject = \"john{}bob\"", '\u{202E}');
        let result = Lexer::tokenize(&input);
        assert!(
            result.is_err(),
            "Bidi chars inside string literals must be rejected (S-1)"
        );
    }

    #[test]
    fn test_bidi_at_start_of_input_rejected_s1() {
        let input = format!("{}RECALL facts", '\u{202A}');
        let result = Lexer::tokenize(&input);
        assert!(
            result.is_err(),
            "Bidi char at start of input must be rejected (S-1)"
        );
    }

    #[test]
    fn test_bidi_at_end_of_input_rejected_s1() {
        let input = format!("RECALL facts{}", '\u{2069}');
        let result = Lexer::tokenize(&input);
        assert!(
            result.is_err(),
            "Bidi char at end of input must be rejected (S-1)"
        );
    }

    // ── 10. Destructive keyword detection ─────────────────────────────────

    #[test]
    fn test_forget_is_not_destructive_keyword() {
        // FORGET is promoted to a first-class CAL statement.
        assert!(!is_destructive_keyword("FORGET"));
        assert!(!is_destructive_keyword("forget"));
        assert!(!is_destructive_keyword("Forget"));
        // DELETE is blocked — use FORGET <hash> instead.
        assert!(is_destructive_keyword("DELETE"));
        assert!(is_destructive_keyword("delete"));
    }

    #[test]
    fn test_drop_and_purge_are_not_destructive_keywords() {
        // DROP and PURGE are promoted to first-class CAL statements.
        assert!(!is_destructive_keyword("DROP"));
        assert!(!is_destructive_keyword("PURGE"));
        assert!(!is_destructive_keyword("RECALL"));
        assert!(!is_destructive_keyword("WHERE"));
    }

    #[test]
    fn test_is_destructive_keyword_crypto_words() {
        assert!(is_destructive_keyword("ENCRYPT"));
        assert!(is_destructive_keyword("DECRYPT"));
        assert!(is_destructive_keyword("ROTATE"));
        assert!(is_destructive_keyword("KEY"));
        assert!(is_destructive_keyword("DEK"));
    }

    #[test]
    fn test_is_destructive_keyword_all_blocked_words() {
        // Exhaustive test of all 25 destructive keywords (FORGET, DROP, PURGE removed; DELETE remains blocked).
        let blocked = [
            "DELETE",
            "ERASE",
            "DESTROY",
            "TRUNCATE",
            "INSERT",
            "CREATE",
            "WRITE",
            "STORE",
            "KEY",
            "ENCRYPT",
            "DECRYPT",
            "ROTATE",
            "MASTER",
            "DEK",
            "SECRET",
            "POLICY",
            "SEAL",
            "UNSEAL",
            "TOKEN",
            "CONSENT",
            "RESTRICT",
            "SCHEMA",
            "PARTITION",
            "INDEX",
            "MIGRATION",
        ];
        for word in &blocked {
            assert!(
                is_destructive_keyword(word),
                "'{}' should be a destructive keyword",
                word
            );
            // Case insensitivity
            assert!(
                is_destructive_keyword(&word.to_lowercase()),
                "'{}' (lowercase) should be destructive",
                word
            );
        }
        // CAL 1.3: GRANT/REVOKE left the blocklist and became Tier-3 DCL
        // keywords — they lex as statement tokens now, not blocked idents.
        assert!(!is_destructive_keyword("GRANT"));
        assert!(!is_destructive_keyword("REVOKE"));
        assert_eq!(
            tok("GRANT revoke Show TO"),
            vec![Token::Grant, Token::Revoke, Token::Show, Token::To]
        );
    }

    #[test]
    fn test_safe_keywords_not_destructive() {
        // Verify that legitimate CAL keywords are not falsely flagged.
        let safe = [
            "RECALL",
            "WHERE",
            "AND",
            "OR",
            "NOT",
            "LIMIT",
            "OFFSET",
            "ORDER",
            "BY",
            "SELECT",
            "COUNT",
            "FIRST",
            "WITH",
            "ABOUT",
            "EXPLAIN",
            "DESCRIBE",
            "BATCH",
            "COALESCE",
            "EXISTS",
            "HISTORY",
            "UNION",
            "INTERSECT",
            "EXCEPT",
            "FORMAT",
        ];
        for word in &safe {
            assert!(
                !is_destructive_keyword(word),
                "'{}' should NOT be a destructive keyword",
                word
            );
        }
    }

    // ── 11. Empty input ───────────────────────────────────────────────────

    #[test]
    fn test_empty_input() {
        let tokens = tok("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_whitespace_only() {
        let tokens = tok("   \t\n  ");
        assert!(tokens.is_empty());
    }

    // ── 12. Unterminated string detection ─────────────────────────────────

    #[test]
    fn test_unterminated_string_error() {
        let err = Lexer::tokenize("\"hello world").unwrap_err();
        assert!(matches!(err, CalError::UnterminatedString { .. }));
    }

    // ── 13. Pipeline operator | ───────────────────────────────────────────

    #[test]
    fn test_pipe_operator() {
        let tokens = tok("RECALL facts | LIMIT 5");
        assert_eq!(
            tokens,
            vec![
                Token::Recall,
                Token::Ident("facts".into()),
                Token::Pipe,
                Token::Limit,
                Token::NumberLiteral(5.0),
            ]
        );
    }

    // ── 14. Whitespace handling ───────────────────────────────────────────

    #[test]
    fn test_whitespace_collapsed() {
        let tokens = tok("RECALL   \t  facts");
        assert_eq!(tokens, vec![Token::Recall, Token::Ident("facts".into())]);
    }

    // ── 15. Combined query tokenization ───────────────────────────────────

    #[test]
    fn test_combined_query_tokenization() {
        let input = r#"RECALL facts WHERE subject = "john" | ORDER BY confidence DESC | LIMIT 10"#;
        let tokens = tok(input);
        assert_eq!(
            tokens,
            vec![
                Token::Recall,
                Token::Ident("facts".into()),
                Token::Where,
                Token::Ident("subject".into()),
                Token::Eq,
                Token::StringLiteral("john".into()),
                Token::Pipe,
                Token::Order,
                Token::By,
                Token::Ident("confidence".into()),
                Token::Desc,
                Token::Pipe,
                Token::Limit,
                Token::NumberLiteral(10.0),
            ]
        );
    }

    // ── 16. Span tracking ─────────────────────────────────────────────────

    #[test]
    fn test_span_tracking() {
        // NOTE: upstream expected end 14 ("beliefs", pre-rename); "facts"
        // spans 7..12. Same latent rename artifact as the comment test.
        let spanned = Lexer::tokenize("RECALL facts").unwrap();
        assert_eq!(spanned[0].span.start, 0);
        assert_eq!(spanned[0].span.end, 6);
        assert_eq!(spanned[1].span.start, 7);
        assert_eq!(spanned[1].span.end, 12);
    }

    #[test]
    fn test_span_tracking_multiple_newlines() {
        // Regression: tokenize used to panic with "attempt to subtract with
        // overflow" when a token was preceded by 2+ newlines, because
        // line_start was mutated inside the loop and used as its own offset
        // base, causing it to overshoot logos_span.start.
        let spanned = Lexer::tokenize("RECALL\nbeliefs\nWHERE subject = \"x\"").unwrap();
        // Find WHERE — should be at line 3, col 1.
        let where_tok = spanned
            .iter()
            .find(|t| matches!(t.token, Token::Where))
            .expect("WHERE token must be present");
        assert_eq!(where_tok.span.line, 3, "WHERE should be on line 3");
        assert_eq!(where_tok.span.col, 1, "WHERE should be at column 1");
    }

    // ── 17. Hash literal with minimum length ─────────────────────────────

    #[test]
    fn test_hash_literal_min_length() {
        let tokens = tok("sha256:abc12345");
        assert_eq!(tokens, vec![Token::HashLiteral("abc12345".into())]);
    }

    // ── 18. NFC normalization (S-6) ───────────────────────────────────────

    #[test]
    fn test_nfc_normalization_idempotent_ascii() {
        // ASCII text is already NFC.
        let result = nfc_normalize("RECALL facts");
        assert_eq!(result, "RECALL facts");
    }

    #[test]
    fn test_nfc_normalization_combines_codepoints_s6() {
        // U+0065 (e) + U+0301 (combining acute) should NFC-normalize to U+00E9 (e with acute).
        let decomposed = "RECALL facts WHERE subject = \"caf\u{0065}\u{0301}\"";
        let precomposed = "RECALL facts WHERE subject = \"caf\u{00E9}\"";
        let norm_decomposed = nfc_normalize(decomposed);
        let norm_precomposed = nfc_normalize(precomposed);
        assert_eq!(
            norm_decomposed, norm_precomposed,
            "NFC equivalents must produce identical normalized forms (S-6)"
        );
    }

    #[test]
    fn test_nfc_equivalent_inputs_same_token_stream_s6() {
        // Two visually identical queries (decomposed vs precomposed) must produce
        // the same token stream after NFC normalization.
        let decomposed = "RECALL facts WHERE subject = \"caf\u{0065}\u{0301}\"";
        let precomposed = "RECALL facts WHERE subject = \"caf\u{00E9}\"";
        let tokens_a = Lexer::tokenize(decomposed).unwrap();
        let tokens_b = Lexer::tokenize(precomposed).unwrap();
        let tok_a: Vec<_> = tokens_a.iter().map(|st| &st.token).collect();
        let tok_b: Vec<_> = tokens_b.iter().map(|st| &st.token).collect();
        assert_eq!(
            tok_a, tok_b,
            "NFC-equivalent inputs must yield identical token streams (S-6)"
        );
    }

    // ── 19. WITH option keywords ──────────────────────────────────────────

    #[test]
    fn test_with_option_keywords() {
        let tokens = tok("WITH SUPERSEDED SCORE_BREAKDOWN EXPLANATION PROVENANCE");
        assert_eq!(
            tokens,
            vec![
                Token::With,
                Token::Superseded,
                Token::ScoreBreakdown,
                Token::Explanation,
                Token::Provenance,
            ]
        );
    }

    // ── 20. Format keywords ───────────────────────────────────────────────

    #[test]
    fn test_format_keywords() {
        let tokens = tok("FORMAT JSON YAML MARKDOWN TEXT SML TOON TRIPLES");
        assert_eq!(
            tokens,
            vec![
                Token::Format,
                Token::Json,
                Token::Yaml,
                Token::Markdown,
                Token::Text,
                Token::Sml,
                Token::Toon,
                Token::Triples,
            ]
        );
    }

    // ── 21. Token description method ─────────────────────────────────────

    #[test]
    fn test_token_description() {
        assert_eq!(Token::Recall.description(), "RECALL");
        assert_eq!(Token::Eq.description(), "=");
        assert_eq!(Token::Ident("foo".into()).description(), "foo");
        assert_eq!(Token::Parameter("bar".into()).description(), "$bar");
        assert_eq!(
            Token::HashLiteral("abc123".into()).description(),
            "sha256:abc123"
        );
    }

    // ── VARS keyword ────────────────────────────────────────────────────

    #[test]
    fn test_vars_keyword() {
        let tokens = tok("VARS");
        assert_eq!(tokens, vec![Token::Vars]);
    }

    #[test]
    fn test_vars_case_insensitive() {
        let tokens = tok("vars Vars VARS vArS");
        assert_eq!(
            tokens,
            vec![Token::Vars, Token::Vars, Token::Vars, Token::Vars]
        );
    }

    #[test]
    fn test_with_vars_sequence() {
        let tokens = tok("WITH VARS");
        assert_eq!(tokens, vec![Token::With, Token::Vars]);
    }

    // ── Template section bodies (OMS CAL §10.6) ───────────────────────────

    /// Pull the single section body out of a one-section input.
    fn body(input: &str) -> String {
        match tok(input).into_iter().next().expect("a token") {
            Token::Header(SectionBody::Body(s))
            | Token::Element(SectionBody::Body(s))
            | Token::ElementSummary(SectionBody::Body(s))
            | Token::ElementOmit(SectionBody::Body(s))
            | Token::SourceBreak(SectionBody::Body(s))
            | Token::Footer(SectionBody::Body(s)) => s,
            other => panic!("expected a section body, got {other:?}"),
        }
    }

    #[test]
    fn test_section_body_is_captured_raw() {
        // Only *newline* layout is stripped; horizontal spacing is content,
        // so a one-line body keeps the spaces the author typed.
        assert_eq!(
            body(r#"ELEMENT { <fact>{{grain.content}}</fact> }"#),
            " <fact>{{grain.content}}</fact> "
        );
    }

    #[test]
    fn test_section_body_strips_one_newline_of_layout_at_each_end() {
        // The spec writes bodies on their own lines; the braces' layout is not
        // part of the rendering.
        assert_eq!(body("HEADER {\n<context>\n  }"), "<context>");
        // A deliberate blank line survives — only one newline is consumed.
        assert_eq!(body("HEADER {\n\n<context>\n\n  }"), "\n<context>\n");
        // Single-line bodies keep their interior spacing.
        assert_eq!(body("FOOTER { x }"), " x ");
    }

    #[test]
    fn test_section_brace_matching_is_mustache_aware() {
        // `{{` / `}}` are interpolation delimiters, not nesting.
        assert_eq!(body("ELEMENT {{{grain.type}}}"), "{{grain.type}}");
        // Single braces nest, so a body may contain a JSON object.
        assert_eq!(
            body(r#"ELEMENT { {"t": "{{grain.type}}"} }"#),
            r#" {"t": "{{grain.type}}"} "#
        );
    }

    #[test]
    fn test_section_body_may_contain_text_that_is_not_cal() {
        // This is the whole reason bodies are captured raw: none of these
        // tokenize as CAL, and before the raw capture each one failed the
        // entire query.
        assert_eq!(body("ELEMENT { don't }"), " don't ");
        assert_eq!(body("ELEMENT { 50% off — \"quoted }"), " 50% off — \"quoted ");
        // A destructive keyword is just prose inside a template body.
        assert_eq!(body("ELEMENT { DELETE the old note }"), " DELETE the old note ");
    }

    #[test]
    fn test_section_keywords_are_still_ordinary_words_without_a_body() {
        // `header` as a field name must keep working.
        assert_eq!(
            tok(r#"WHERE header = "x""#),
            vec![
                Token::Where,
                Token::Header(SectionBody::Bare),
                Token::Eq,
                Token::StringLiteral("x".into()),
            ]
        );
    }

    #[test]
    fn test_unterminated_section_body_is_left_for_the_parser() {
        // No closing brace: the lexer must not swallow the rest of the query
        // or fail outright — the parser reports it with a span.
        assert_eq!(
            tok("ELEMENT { unclosed"),
            vec![
                Token::Element(SectionBody::Bare),
                Token::LBrace,
                Token::Ident("unclosed".into()),
            ]
        );
    }

    #[test]
    fn test_all_six_sections_lex() {
        let toks = tok("HEADER {a} ELEMENT {b} ELEMENT_SUMMARY {c} \
                        ELEMENT_OMIT {d} SOURCE_BREAK {e} FOOTER {f}");
        assert_eq!(
            toks,
            vec![
                Token::Header(SectionBody::Body("a".into())),
                Token::Element(SectionBody::Body("b".into())),
                Token::ElementSummary(SectionBody::Body("c".into())),
                Token::ElementOmit(SectionBody::Body("d".into())),
                Token::SourceBreak(SectionBody::Body("e".into())),
                Token::Footer(SectionBody::Body("f".into())),
            ]
        );
    }
}

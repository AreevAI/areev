//! CAL recursive-descent parser.
//!
//! Transforms a flat `Vec<SpannedToken>` (from [`super::lexer::Lexer`]) into a
//! typed [`super::ast::CalQuery`] AST.
//!
//! # Design principles
//!
//! - **All 12 statement types are parsed** even though Phase 1 only executes
//!   `RECALL` and `EXISTS`.  This ensures that unimplemented-but-valid CAL
//!   queries produce a useful "not yet supported" message instead of a cryptic
//!   parse error.
//! - **Resource limits** are checked during parsing so that pathological
//!   queries are rejected before any engine work is done.
//! - **Destructive keyword guard**: any identifier that matches a blocked word
//!   (see [`super::lexer::is_destructive_keyword`]) is rejected immediately
//!   with a clear diagnostic.
//! - **Error messages** always include a [`Span`] and, where possible, a
//!   `suggestion` to help the user correct the query.
//!
//! # Entry points
//!
//! ```text
//! let query = parse("RECALL facts WHERE subject = \"john\"")?;
//! let query = parse_with_params(input, &params)?;
//! ```

use std::collections::HashMap;

use super::ast::{
    AboutClause, AccumulateStmt, AccumulateTarget, AddStmt, AddWithOption, AddWorkflowStmt,
    AliasedFormat, AssembleStmt, AssembleWithOption, BatchEntry, BatchStmt, BetweenClause,
    BindClause, BudgetSpec, BudgetUnit, CalQuery, CalStatement, CalVersion, CoalesceBranch,
    CoalesceStmt, Comparator, Condition, ContradictionsClause, DefineTemplateStmt, DeltaOp,
    DescribeStmt, DescribeTarget, ExistsStmt, ExplainStmt, Extractor, FieldAssignment,
    FormatClause, FormatSpec, GrainTypePlural, GrainTypeSingular, GraphEdge, HistoryStmt,
    LetBinding, LikeClause, NamedSource, PipelineStage, PrioritySpec, ProjectField, RecallStmt,
    RecentClause, RevertStmt, SetOp, SetOpStmt, Source, SupersedeStmt, SupersedeWorkflowStmt,
    UntilClause, Value, WhereClause, WithOption,
};
use super::ast::{
    DefineQueryStmt, DerivedFromStmt, DropQueryStmt, EntityAtStmt, ForgetStmt, ForgetTarget,
    GovernanceStmt, GrantStmt, MergeStmt, NoveltyStmt, PurgeStmt, QueryParam, RelatedStmt,
    RememberStmt, ReportSubjectStmt, RevokeStmt, RunLoopStmt, RunQueryStmt, RunTraceStmt,
    RunsTouchingStmt, ShowForksStmt, ShowGrantsStmt, TemplateSectionSources,
};
use super::errors::{CalError, CalResult, CalWarning, Span};
use super::lexer::{is_destructive_keyword, Lexer, SectionBody, SpannedToken, Token};

/// The parsed pieces of a GRANT/REVOKE body:
/// `(verbs, namespaces, principal, reason)`.
type DclBody = (Vec<String>, Vec<String>, String, Option<String>);

/// Parse a `RUN LOOP WITH if_stale("…")` duration — `"300s"`, `"90m"`,
/// `"6h"`, `"2d"` — to milliseconds. `None` on anything else.
fn parse_stale_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: f64 = num.parse().ok()?;
    if n < 0.0 || !n.is_finite() {
        return None;
    }
    let ms = match unit {
        "s" => n * 1000.0,
        "m" => n * 60_000.0,
        "h" => n * 3_600_000.0,
        "d" => n * 86_400_000.0,
        _ => return None,
    };
    Some(ms as i64)
}

// ---------------------------------------------------------------------------
// Hard limits
// ---------------------------------------------------------------------------

/// Maximum CAL query byte length (prevents memory exhaustion before lexing).
/// 64 KB accommodates agent system prompts, goal descriptions, and multi-turn
/// context while still preventing abuse.  Aligned with embedding model context
/// windows (~16K tokens ≈ 64 KB).
pub(crate) const MAX_QUERY_LENGTH: usize = 65_536;

/// Maximum nesting depth (parentheses, sub-queries).
///
/// Shared with the JSON wire-format pre-validator in `json.rs`. Set-op
/// chains and pipelines cap independently at 4 / 5.
pub(crate) const MAX_NESTING_DEPTH: usize = 8;

/// Maximum value for an inline `LIMIT` or `| LIMIT n`.
const MAX_LIMIT_VALUE: u64 = 1_000;

/// Maximum number of values in an `IN (...)` set.
const MAX_IN_SET_SIZE: usize = 100;

/// Maximum pipeline stages in a single query.
const MAX_PIPELINE_STAGES: usize = 5;

/// Maximum operands in a UNION / INTERSECT / EXCEPT chain.
const MAX_SET_OPERANDS: usize = 4;

/// Maximum statements inside a `BATCH { ... }`.
const MAX_BATCH_ENTRIES: usize = 10;

/// Maximum byte length for a `REASON "..."` string.
const MAX_REASON_LENGTH: usize = 500;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Parse a CAL query string into a typed AST.
///
/// Applies the full pipeline: length check → bidi check → NFC normalization →
/// lexing → recursive-descent parsing → limit validation.
///
/// # Errors
///
/// Any [`CalError`] variant.  Error messages include source spans and, where
/// possible, a `suggestion`.
pub fn parse(input: &str) -> CalResult<CalQuery> {
    parse_with_params(input, &HashMap::new())
}

/// Parse a CAL query with pre-bound parameter values.
///
/// Parameters in the query (`$name`) that appear in `params` are *not*
/// validated at parse time (that is the executor's job); this call merely
/// makes them available for future inline substitution.
///
/// All other validation rules (length, bidi, nesting, limits) still apply.
pub fn parse_with_params(input: &str, _params: &HashMap<String, Value>) -> CalResult<CalQuery> {
    // Length check first — before any allocation.
    if input.len() > MAX_QUERY_LENGTH {
        return Err(CalError::QueryTooLong {
            length: input.len(),
            max: MAX_QUERY_LENGTH,
            span: None,
        });
    }

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CalError::EmptyQuery { span: None });
    }

    // Lex (includes bidi check and NFC normalisation).
    let tokens = Lexer::tokenize(input)?;

    if tokens.is_empty() {
        return Err(CalError::EmptyQuery {
            span: Some(Span::zero()),
        });
    }

    let mut parser = Parser::new(tokens, input);
    parser.parse_query()
}

/// Rewrite every `$param` reference to a bare literal so a body whose
/// parameters sit in positions that demand one (`RECENT $limit`) can still be
/// parsed for SHAPE at DEFINE time. Only ever fed to the parser — never stored,
/// never executed.
///
/// String literals are stepped over: a `"$5.00"` in a WHERE value is data, not
/// a parameter, and rewriting it could fail a body that is perfectly valid.
fn params_as_literals(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == '\\' {
                    // The escaped character cannot close the literal.
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                out.push(c);
            }
            None if c == '$' => {
                while chars.peek().is_some_and(|n| n.is_ascii_alphanumeric() || *n == '_') {
                    chars.next();
                }
                // `1` parses in both a numeric and a value position; the type
                // mismatch it can imply (CAL-E020) belongs to the executor, so
                // a shape check never sees it.
                out.push('1');
            }
            None => out.push(c),
        }
    }
    out
}

/// Recursively check that a statement is read-only (no writes, no RUN) — the
/// enforcement behind the "saved-query bodies stay read-only" invariant. A free
/// function so the executor can re-check a parsed RUN body at execution time,
/// not only the parser at DEFINE time (defense in depth: the DEFINE-time scan
/// runs a word-level guard BEFORE the parse, so it holds even for a body the
/// parser has to see through placeholders).
pub(crate) fn check_read_only_statement(stmt: &CalStatement, span: &Span) -> CalResult<()> {
    let write = |s: &str| {
        Err(CalError::WriteInQueryBody {
            stmt: s.into(),
            span: Some(*span),
        })
    };
    match stmt {
        CalStatement::Recall(_)
        | CalStatement::SetOp(_)
        | CalStatement::Exists(_)
        | CalStatement::Assemble(_)
        | CalStatement::History(_)
        | CalStatement::Explain(_)
        | CalStatement::Describe(_)
        | CalStatement::Coalesce(_) => Ok(()),
        CalStatement::Batch(batch) => {
            for entry in &batch.statements {
                check_read_only_statement(&entry.statement, span)?;
            }
            if let Some(labeled) = &batch.labeled {
                for (_, entry) in labeled {
                    check_read_only_statement(&entry.statement, span)?;
                }
            }
            Ok(())
        }
        CalStatement::Add(_) | CalStatement::AddWorkflow(_) => write("ADD"),
        CalStatement::Supersede(_) | CalStatement::SupersedeWorkflow(_) => write("SUPERSEDE"),
        CalStatement::Accumulate(_) => write("ACCUMULATE"),
        CalStatement::Revert(_) => write("REVERT"),
        CalStatement::Remember(_) => write("REMEMBER"),
        CalStatement::EntityAt(_)
        | CalStatement::RunTrace(_)
        | CalStatement::RunsTouching(_)
        | CalStatement::DerivedFrom(_)
        | CalStatement::ShowForks(_)
        | CalStatement::Related(_)
        | CalStatement::Novelty(_)
        | CalStatement::ReportSubject(_) => Ok(()),
        CalStatement::Merge(_) => write("MERGE"),
        CalStatement::Forget(_) => write("FORGET"),
        CalStatement::Purge(_) => write("PURGE"),
        // No DCL inside saved-query bodies — a stored GRANT executing with
        // the invoker's rights would be a confused deputy. SHOW GRANTS is a
        // read and fine.
        CalStatement::Grant(_) => write("GRANT"),
        CalStatement::Revoke(_) => write("REVOKE"),
        CalStatement::ShowGrants(_) => Ok(()),
        // No governance in stored bodies either — the deliberate friction
        // of one-statement-at-a-time review must not be macro-able.
        CalStatement::Approve(_) => write("APPROVE"),
        CalStatement::Reject(_) => write("REJECT"),
        CalStatement::ApplyRec(_) => write("APPLY"),
        CalStatement::RollbackRec(_) => write("ROLLBACK"),
        CalStatement::RunLoop(_) => write("RUN LOOP"),
        CalStatement::DefineTemplate(_) => write("DEFINE TEMPLATE"),
        CalStatement::DropTemplate(_) => write("DROP TEMPLATE"),
        CalStatement::DefineQuery(_) => write("DEFINE QUERY"),
        CalStatement::DropQuery(_) => write("DROP QUERY"),
        CalStatement::RunQuery(_) => Err(CalError::RecursiveQuery { span: Some(*span) }),
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Internal recursive-descent parser state.
struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    warnings: Vec<CalWarning>,
    nesting_depth: usize,
    /// Original input text — used by `reconstruct_body_text` for DEFINE QUERY bodies.
    input: String,
}

impl Parser {
    fn new(tokens: Vec<SpannedToken>, input: &str) -> Self {
        Self {
            tokens,
            pos: 0,
            warnings: vec![],
            nesting_depth: 0,
            input: input.to_string(),
        }
    }

    // -- Cursor helpers ----------------------------------------------------

    /// Peek at the current token without consuming it.
    fn peek(&self) -> Option<&SpannedToken> {
        self.tokens.get(self.pos)
    }

    /// Peek at the token `offset` positions ahead (0 = current).
    fn peek_ahead(&self, offset: usize) -> Option<&SpannedToken> {
        self.tokens.get(self.pos + offset)
    }

    /// Consume and return the current token.
    fn advance(&mut self) -> Option<&SpannedToken> {
        if self.pos < self.tokens.len() {
            let tok = &self.tokens[self.pos];
            self.pos += 1;
            Some(tok)
        } else {
            None
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// Return the span of the current token (or zero if at end).
    fn current_span(&self) -> Span {
        self.peek().map(|st| st.span).unwrap_or_else(Span::zero)
    }

    /// Return the span of the previous token (or zero if at start).
    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            Span::zero()
        } else {
            self.tokens
                .get(self.pos - 1)
                .map(|st| st.span)
                .unwrap_or_else(Span::zero)
        }
    }

    /// Consume the current token if it matches `token` (by discriminant).
    /// Returns `true` if consumed.
    #[allow(dead_code)] // Reserved for Phase 3 parser features.
    fn eat_if_token(&mut self, token: &Token) -> bool {
        if let Some(st) = self.peek() {
            if std::mem::discriminant(&st.token) == std::mem::discriminant(token) {
                self.advance();
                return true;
            }
        }
        false
    }

    /// Consume the current token if it is exactly `token` (value equality).
    fn eat_exact(&mut self, token: &Token) -> bool {
        if let Some(st) = self.peek() {
            if &st.token == token {
                self.advance();
                return true;
            }
        }
        false
    }

    /// Require the next token to be exactly `expected` (value equality);
    /// consume and return it, or return an error.
    fn expect_exact(&mut self, expected: &Token) -> CalResult<SpannedToken> {
        match self.peek() {
            Some(st) if &st.token == expected => {
                let st = st.clone();
                self.advance();
                Ok(st)
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: expected.description(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: expected.description(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Require the next token to match `expected` by discriminant.
    #[allow(dead_code)] // Reserved for Phase 3 parser features.
    fn expect_token(&mut self, expected: &Token) -> CalResult<SpannedToken> {
        match self.peek() {
            Some(st) if std::mem::discriminant(&st.token) == std::mem::discriminant(expected) => {
                let st = st.clone();
                self.advance();
                Ok(st)
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: expected.description(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: expected.description(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Check whether the current token matches `expected` by discriminant
    /// (without consuming).
    fn at(&self, expected: &Token) -> bool {
        match self.peek() {
            Some(st) => std::mem::discriminant(&st.token) == std::mem::discriminant(expected),
            None => false,
        }
    }

    /// Check whether the current token is exactly `expected` (value equality).
    fn at_exact(&self, expected: &Token) -> bool {
        self.peek().map(|st| &st.token == expected).unwrap_or(false)
    }

    // -- Nesting depth guard ----------------------------------------------

    fn enter_nesting(&mut self) -> CalResult<()> {
        self.nesting_depth += 1;
        if self.nesting_depth > MAX_NESTING_DEPTH {
            return Err(CalError::NestingTooDeep {
                depth: self.nesting_depth,
                max: MAX_NESTING_DEPTH,
                span: Some(self.current_span()),
            });
        }
        Ok(())
    }

    fn leave_nesting(&mut self) {
        self.nesting_depth = self.nesting_depth.saturating_sub(1);
    }

    // -- Literal helpers --------------------------------------------------

    fn parse_string_literal(&mut self) -> CalResult<String> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::StringLiteral(_),
                ..
            }) => {
                let st = self.advance().unwrap();
                if let Token::StringLiteral(s) = &st.token {
                    Ok(s.clone())
                } else {
                    unreachable!()
                }
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "string literal".into(),
                    found,
                    span: Some(span),
                    suggestion: Some("string values must be enclosed in double quotes".into()),
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "string literal".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    fn parse_number(&mut self) -> CalResult<f64> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::NumberLiteral(_),
                ..
            }) => {
                let st = self.advance().unwrap();
                if let Token::NumberLiteral(n) = &st.token {
                    Ok(*n)
                } else {
                    unreachable!()
                }
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "number literal".into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "number literal".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    fn parse_u64(&mut self) -> CalResult<u64> {
        let span = self.current_span();
        let n = self.parse_number()?;
        if n < 0.0 || n.fract() != 0.0 {
            return Err(CalError::InvalidNumber {
                found: n.to_string(),
                span: Some(span),
            });
        }
        Ok(n as u64)
    }

    fn parse_hash_literal(&mut self) -> CalResult<String> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::HashLiteral(_),
                ..
            }) => {
                let st = self.advance().unwrap();
                if let Token::HashLiteral(h) = &st.token {
                    Ok(h.clone())
                } else {
                    unreachable!()
                }
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "hash literal (sha256:...)".into(),
                    found,
                    span: Some(span),
                    suggestion: Some("hash literals use the format sha256:<hex digits>".into()),
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "hash literal (sha256:...)".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Parse a `$parameter` token and return its name.
    fn parse_parameter(&mut self) -> CalResult<String> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Parameter(_),
                ..
            }) => {
                let st = self.advance().unwrap();
                if let Token::Parameter(p) = &st.token {
                    Ok(p.clone())
                } else {
                    unreachable!()
                }
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "$parameter".into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "$parameter".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Parse any single scalar value (string, number, bool, hash, parameter).
    fn parse_value(&mut self) -> CalResult<Value> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::StringLiteral(_),
                ..
            }) => Ok(Value::String {
                value: self.parse_string_literal()?,
            }),
            Some(SpannedToken {
                token: Token::NumberLiteral(_),
                ..
            }) => Ok(Value::Number {
                value: self.parse_number()?,
            }),
            Some(SpannedToken {
                token: Token::True, ..
            }) => {
                self.advance();
                Ok(Value::Boolean { value: true })
            }
            Some(SpannedToken {
                token: Token::False,
                ..
            }) => {
                self.advance();
                Ok(Value::Boolean { value: false })
            }
            Some(SpannedToken {
                token: Token::HashLiteral(_),
                ..
            }) => Ok(Value::Hash {
                value: self.parse_hash_literal()?,
            }),
            Some(SpannedToken {
                token: Token::Parameter(_),
                ..
            }) => Ok(Value::Parameter {
                name: self.parse_parameter()?,
            }),
            // Array literal: [ v, v, ... ]
            Some(SpannedToken {
                token: Token::LBracket,
                ..
            }) => {
                self.advance(); // consume [
                self.enter_nesting()?;
                let mut values = vec![];
                while !self.at_exact(&Token::RBracket) && !self.at_end() {
                    values.push(self.parse_value()?);
                    if !self.eat_exact(&Token::Comma) {
                        break;
                    }
                }
                self.leave_nesting();
                self.expect_exact(&Token::RBracket)?;
                Ok(Value::Array { values })
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "a value (string, number, boolean, hash, $parameter, or [array])"
                        .into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "a value".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Parse a comma-separated list of values enclosed in `(` ... `)`.
    fn parse_value_list(&mut self) -> CalResult<Vec<Value>> {
        self.expect_exact(&Token::LParen)?;
        self.enter_nesting()?;
        let mut values = vec![];
        while !self.at_exact(&Token::RParen) && !self.at_end() {
            // Also accept $param as a shorthand for a whole parameter list.
            if self.at(&Token::Parameter("".into())) {
                // A single bare $param stands for an entire list.
                let name = self.parse_parameter()?;
                values.push(Value::Parameter { name });
            } else {
                values.push(self.parse_value()?);
            }
            if !self.eat_exact(&Token::Comma) {
                break;
            }
        }
        self.leave_nesting();
        self.expect_exact(&Token::RParen)?;
        // OMS §4 `value_list = value , { "," , value }` requires ≥1 element.
        if values.is_empty() {
            return Err(CalError::UnexpectedToken {
                expected: "at least one value inside IN ( ... )".into(),
                found: "empty IN list".into(),
                span: Some(self.prev_span()),
                suggestion: Some("IN requires one or more values.".into()),
            });
        }
        if values.len() > MAX_IN_SET_SIZE {
            return Err(CalError::InSetTooLarge {
                count: values.len(),
                max: MAX_IN_SET_SIZE,
                span: Some(self.prev_span()),
            });
        }
        Ok(values)
    }

    /// Parse an unquoted identifier (Ident token).
    ///
    /// Also accepts workflow-graph keywords (`ON`, `WHEN`, `BIND`) as
    /// identifiers so they remain usable as field names in non-workflow
    /// contexts (e.g. `WHERE on = "value"`).
    fn parse_identifier(&mut self) -> CalResult<String> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Ident(_),
                ..
            }) => {
                let st = self.advance().unwrap();
                if let Token::Ident(s) = &st.token {
                    // Destructive keyword guard.
                    if is_destructive_keyword(s) {
                        return Err(CalError::UnexpectedToken {
                            expected: "a field name or grain type".into(),
                            found: s.clone(),
                            span: Some(self.prev_span()),
                            suggestion: Some(
                                "CAL cannot destroy data in bulk. The only \
                                 destructive CAL statement is FORGET <hash>; \
                                 erasure, key rotation, and schema changes are \
                                 host-level operations."
                                    .into(),
                            ),
                        });
                    }
                    Ok(s.clone())
                } else {
                    unreachable!()
                }
            }
            // Keywords that are also valid field names in WHERE/SELECT
            // context (e.g. `WHERE on = "value"`, `WHERE priority = "high"`,
            // `SELECT scope`).  Without this arm, the lexer's keyword
            // matching would make these unusable as field names.
            Some(SpannedToken {
                token: Token::On | Token::When | Token::Bind | Token::Priority | Token::Scope,
                ..
            }) => {
                let st = self.advance().unwrap();
                Ok(st.token.description().to_ascii_lowercase())
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "identifier".into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "identifier".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Parse a label name — like `parse_identifier` but also accepts keyword
    /// tokens.  Used in contexts where a keyword can serve as a user-chosen
    /// name (e.g. multi-source ASSEMBLE labels, PRIORITY labels).
    fn parse_label(&mut self) -> CalResult<String> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Ident(_),
                ..
            }) => {
                let st = self.advance().unwrap();
                if let Token::Ident(s) = &st.token {
                    Ok(s.clone())
                } else {
                    unreachable!()
                }
            }
            // Accept any keyword token as a label.
            Some(st) if Self::is_word_token(&st.token) => {
                let label = st.token.description().to_ascii_lowercase();
                self.advance();
                Ok(label)
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "label name".into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "label name".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Returns true if the token is a word-like keyword that could serve as
    /// a label in multi-source ASSEMBLE or PRIORITY clauses.
    fn is_word_token(token: &Token) -> bool {
        !matches!(
            token,
            Token::Eq
                | Token::NotEq
                | Token::Gte
                | Token::Lte
                | Token::Gt
                | Token::Lt
                | Token::LParen
                | Token::RParen
                | Token::LBracket
                | Token::RBracket
                | Token::LBrace
                | Token::RBrace
                | Token::Pipe
                | Token::Comma
                | Token::Colon
                | Token::Dot
                | Token::Slash
                | Token::NumberLiteral(_)
                | Token::StringLiteral(_)
                | Token::HashLiteral(_)
                | Token::Parameter(_)
        )
    }

    /// Parse a plural grain type name from an `Ident` token, if present.
    ///
    /// Returns `None` (without consuming) if the current token is not a
    /// plural grain-type word.
    fn parse_grain_type_plural_opt(&mut self) -> CalResult<Option<GrainTypePlural>> {
        // Check if the current Ident looks like a grain type.
        if let Some(SpannedToken {
            token: Token::Ident(s),
            span,
            ..
        }) = self.peek()
        {
            let s = s.clone();
            let span = *span;
            if let Some(gt) = GrainTypePlural::parse(&s) {
                self.advance();
                return Ok(Some(gt));
            }
            // Check for the common mistake of using singular or old OMS 1.1 names.
            let suggestion = suggest_grain_type_plural(&s);
            // Only error if this looks like it should be a grain type.
            if suggestion.is_some() {
                return Err(CalError::UnknownGrainType {
                    found: s,
                    span: Some(span),
                    suggestion,
                });
            }
        }
        // "*" wildcard.
        if self.at_exact(&Token::All) {
            self.advance();
            return Ok(Some(GrainTypePlural::All));
        }
        Ok(None)
    }

    /// Parse a required plural grain type name.
    fn parse_grain_type_plural(&mut self) -> CalResult<GrainTypePlural> {
        let span = self.current_span();
        // Check for bare identifier.
        match self.peek() {
            Some(SpannedToken {
                token: Token::Ident(s),
                ..
            }) => {
                let s = s.clone();
                // Reject singular forms — RECALL position requires plural per spec EBNF.
                let lower = s.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "fact"
                        | "event"
                        | "state"
                        | "workflow"
                        | "tool"
                        | "observation"
                        | "goal"
                        | "reasoning"
                        | "consensus"
                        | "consent"
                ) {
                    let plural = match lower.as_str() {
                        "fact" => "facts",
                        "event" => "events",
                        "state" => "states",
                        "workflow" => "workflows",
                        "tool" => "tools",
                        "observation" => "observations",
                        "goal" => "goals",
                        "reasoning" => "reasonings",
                        "consensus" => "consensuses",
                        "consent" => "consents",
                        _ => unreachable!(),
                    };
                    return Err(CalError::UnknownGrainType {
                        found: s,
                        span: Some(span),
                        suggestion: Some(format!(
                            "did you mean \"{}\"? RECALL requires the plural form.",
                            plural
                        )),
                    });
                }
                if let Some(gt) = GrainTypePlural::parse(&s) {
                    self.advance();
                    return Ok(gt);
                }
                let suggestion = suggest_grain_type_plural(&s);
                Err(CalError::UnknownGrainType {
                    found: s,
                    span: Some(span),
                    suggestion,
                })
            }
            Some(SpannedToken {
                token: Token::All, ..
            }) => {
                self.advance();
                Ok(GrainTypePlural::All)
            }
            Some(st) => {
                let found = st.token.description();
                Err(CalError::UnexpectedToken {
                    expected: "grain type (facts, events, states, ...)".into(),
                    found,
                    span: Some(span),
                    suggestion: Some(
                        "valid grain types: facts, events, states, workflows, tools, \
                         observations, goals, reasonings, consensuses, consents, skills, *"
                            .into(),
                    ),
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "grain type".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Parse a required singular grain type name (used in ADD).
    fn parse_grain_type_singular(&mut self) -> CalResult<GrainTypeSingular> {
        let span = self.current_span();
        match self.peek() {
            Some(SpannedToken {
                token: Token::Ident(s),
                ..
            }) => {
                let s = s.clone();
                if let Some(gt) = GrainTypeSingular::parse(&s) {
                    self.advance();
                    return Ok(gt);
                }
                let suggestion = suggest_grain_type_singular(&s);
                Err(CalError::UnknownGrainType {
                    found: s,
                    span: Some(span),
                    suggestion,
                })
            }
            // "observation" is a keyword token (relation-category), so the
            // lexer emits Token::Observation instead of Token::Ident.  Handle
            // it explicitly so `ADD observation ...` works correctly.
            Some(SpannedToken {
                token: Token::Observation,
                ..
            }) => {
                self.advance();
                Ok(GrainTypeSingular::Observation)
            }
            Some(st) => {
                let found = st.token.description();
                Err(CalError::UnexpectedToken {
                    expected: "grain type (fact, event, state, ...)".into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "grain type".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    // -- Top-level parse --------------------------------------------------

    /// Parse a complete `CalQuery`.
    ///
    /// Grammar:
    /// ```text
    /// query = [version_prefix] [let_block] statement [pipeline] [with_clause] [format_clause]
    /// ```
    fn parse_query(&mut self) -> CalResult<CalQuery> {
        // Optional version prefix: `CAL/1`.
        let version = self.parse_version_prefix()?;

        // Optional LET bindings.
        let let_bindings = self.parse_let_block()?;

        // Core statement.
        let (statement, inline_pipeline, inline_with, inline_format, user_vars) =
            self.parse_statement_full()?;

        // A query is exactly one statement and must consume ALL input.
        // Trailing semicolons are tolerated (SQL habit), but any other
        // trailing token is an error — silently dropping it would partially
        // execute what the caller sent (e.g. `RECALL ...; <second stmt>`).
        while self.at_exact(&Token::Semicolon) {
            self.advance();
        }
        if let Some(st) = self.peek() {
            let found = st.token.description();
            let span = st.span;
            return Err(CalError::UnexpectedToken {
                expected: "end of query".into(),
                found,
                span: Some(span),
                suggestion: Some(
                    "a query is one statement; use BATCH { stmt1 ; stmt2 } to run several".into(),
                ),
            });
        }

        Ok(CalQuery {
            // Execution state — the executor fills this once LET has run.
            let_values: Default::default(),
            version,
            statement,
            pipeline: inline_pipeline,
            with_options: inline_with,
            format: inline_format,
            let_bindings,
            user_vars,
            warnings: self.warnings.clone(),
        })
    }

    /// Parse an optional `CAL/<n>` version prefix.
    fn parse_version_prefix(&mut self) -> CalResult<CalVersion> {
        if !self.at_exact(&Token::Cal) {
            return Ok(CalVersion::default());
        }
        let span = self.current_span();
        self.advance(); // consume CAL
                        // Expect `/`
        self.expect_exact(&Token::Slash)?;
        // Expect the version number.
        let n = self.parse_u64()?;
        if n != 1 {
            return Err(CalError::UnsupportedVersion {
                version: n as u32,
                span: Some(span),
            });
        }
        Ok(CalVersion(n as u32))
    }

    /// Parse zero or more `LET $name = ... ;` bindings.
    fn parse_let_block(&mut self) -> CalResult<Vec<LetBinding>> {
        let mut bindings = vec![];
        while self.at_exact(&Token::Let) {
            let span_start = self.current_span();
            self.advance(); // consume LET
            let name = self.parse_parameter()?;
            self.expect_exact(&Token::Eq)?;

            // Check for the `EXTRACTOR OF (RECALL ...)` form:
            //   LET $users = SUBJECTS OF (RECALL facts WHERE ...)
            // where the extractor precedes the recall statement.
            let (source_stmt, extractor) = if self.at_extractor() {
                let extractor = self.parse_extractor()?;
                // Optional OF keyword (Token::Of or legacy Ident "OF").
                if matches!(
                    self.peek(),
                    Some(SpannedToken {
                        token: Token::Of,
                        ..
                    })
                ) {
                    self.advance();
                } else if let Some(SpannedToken {
                    token: Token::Ident(id),
                    ..
                }) = self.peek()
                {
                    if id.eq_ignore_ascii_case("OF") {
                        self.advance();
                    }
                }
                // Sub-query may be parenthesised or bare RECALL.
                let source_stmt = if self.at_exact(&Token::LParen) {
                    self.advance();
                    self.enter_nesting()?;
                    let stmt = self.parse_recall_stmt()?;
                    self.leave_nesting();
                    self.expect_exact(&Token::RParen)?;
                    stmt
                } else {
                    self.parse_recall_stmt()?
                };
                (source_stmt, extractor)
            } else {
                // Original form: `LET $x = RECALL ... SUBJECTS`
                let source_stmt = self.parse_recall_stmt()?;

                // Optional `SUBJECTS` / `OBJECTS` / `HASHES` extractor (pipe optional).
                let extractor = if self.at_exact(&Token::Pipe) || self.at_extractor() {
                    if self.eat_exact(&Token::Pipe) {
                        self.warnings.push(CalWarning::DeprecatedPipeOperator {
                            span: Some(self.current_span()),
                        });
                    }
                    self.parse_extractor()?
                } else {
                    Extractor::Hashes // default — callers can specify
                };
                (source_stmt, extractor)
            };

            self.expect_exact(&Token::Semicolon)?;

            let span_end = self.prev_span();
            let span = Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            );

            bindings.push(LetBinding {
                name,
                extractor,
                source: Box::new(CalStatement::Recall(source_stmt)),
                span: Some(span),
            });
        }
        Ok(bindings)
    }

    /// Check whether the current token starts an extractor.
    fn at_extractor(&self) -> bool {
        matches!(
            self.peek(),
            Some(SpannedToken {
                token: Token::Subjects | Token::Objects | Token::Hashes,
                ..
            })
        )
    }

    fn parse_extractor(&mut self) -> CalResult<Extractor> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Subjects,
                ..
            }) => {
                self.advance();
                Ok(Extractor::Subjects)
            }
            Some(SpannedToken {
                token: Token::Objects,
                ..
            }) => {
                self.advance();
                Ok(Extractor::Objects)
            }
            Some(SpannedToken {
                token: Token::Hashes,
                ..
            }) => {
                self.advance();
                Ok(Extractor::Hashes)
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "SUBJECTS, OBJECTS, or HASHES".into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "SUBJECTS, OBJECTS, or HASHES".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Parse a statement and its trailing pipeline / WITH / FORMAT clauses.
    ///
    /// Clause ordering is flexible: WITH options, FORMAT, WITH VARS, and
    /// post-pipeline WHERE can appear in any order after the pipeline stages.
    #[allow(clippy::type_complexity)]
    fn parse_statement_full(
        &mut self,
    ) -> CalResult<(
        CalStatement,
        Vec<PipelineStage>,
        Vec<WithOption>,
        Option<FormatClause>,
        HashMap<String, String>,
    )> {
        let stmt = self.parse_statement()?;

        // After the statement body, check for pipeline.
        let mut pipeline = self.parse_pipeline()?;

        // Parse trailing clauses in any order: WITH options, FORMAT, WITH VARS,
        // and post-pipeline WHERE filters.
        let mut with_options: Vec<WithOption> = Vec::new();
        let mut format = None;
        let mut user_vars = HashMap::new();

        loop {
            if self.at_exact(&Token::Format) && format.is_none() {
                format = Some(self.parse_format()?);
            } else if self.at_exact(&Token::As) && format.is_none() && !self.peek_next_is_of() {
                format = Some(self.parse_as_format()?);
            } else if self.at_exact(&Token::With) {
                if self.peek_next_is_vars() {
                    if user_vars.is_empty() {
                        user_vars = self.parse_user_vars()?;
                    } else {
                        break;
                    }
                } else if with_options.is_empty() {
                    with_options = self.parse_with_clause()?;
                } else {
                    // Merge additional WITH clauses instead of silently ignoring them.
                    let extra = self.parse_with_clause()?;
                    with_options.extend(extra);
                }
            } else if self.at_exact(&Token::Where) {
                // WHERE after pipeline stages — add as a post-pipeline filter.
                let wc = self.parse_where_clause()?;
                if let Some(clause) = wc {
                    pipeline.push(PipelineStage::Filter {
                        condition: clause.condition,
                        span: clause.span,
                    });
                }
            } else {
                break;
            }
        }

        Ok((stmt, pipeline, with_options, format, user_vars))
    }

    // -- Statement dispatch -----------------------------------------------

    fn parse_statement(&mut self) -> CalResult<CalStatement> {
        // Destructive keyword fast-reject before dispatch.
        if let Some(SpannedToken {
            token: Token::Ident(word),
            span,
            ..
        }) = self.peek()
        {
            if is_destructive_keyword(word) {
                let word = word.clone();
                let span = *span;
                return Err(CalError::UnexpectedToken {
                    expected: "RECALL, EXISTS, ASSEMBLE, HISTORY, EXPLAIN, DESCRIBE, BATCH, COALESCE, ADD, SUPERSEDE, REVERT, FORGET, DEFINE, DROP, or STREAM".into(),
                    found: word,
                    span: Some(span),
                    suggestion: Some(
                        "CAL cannot destroy data in bulk. The only destructive \
                         CAL statement is FORGET <hash>; erasure, key rotation, \
                         and schema changes are host-level operations."
                            .into(),
                    ),
                });
            }
        }

        match self.peek() {
            Some(SpannedToken {
                token: Token::Explain,
                ..
            }) => self.parse_explain(),
            Some(SpannedToken {
                token: Token::Recall,
                ..
            }) => {
                let stmt = self.parse_recall_stmt()?;
                // Check for set operation.
                self.parse_set_op_tail(CalStatement::Recall(stmt))
            }
            Some(SpannedToken {
                token: Token::Assemble,
                ..
            }) => self.parse_assemble(),
            Some(SpannedToken {
                token: Token::Exists,
                ..
            }) => self.parse_exists(),
            Some(SpannedToken {
                token: Token::History,
                ..
            }) => self.parse_history(),
            Some(SpannedToken {
                token: Token::Describe,
                ..
            }) => self.parse_describe(),
            Some(SpannedToken {
                token: Token::Batch,
                ..
            }) => self.parse_batch(),
            Some(SpannedToken {
                token: Token::Coalesce,
                ..
            }) => self.parse_coalesce(),
            Some(SpannedToken {
                token: Token::Add, ..
            }) => self.parse_add(),
            Some(SpannedToken {
                token: Token::Accumulate,
                ..
            }) => self.parse_accumulate(),
            Some(SpannedToken {
                token: Token::Supersede,
                ..
            }) => self.parse_supersede(),
            Some(SpannedToken {
                token: Token::Revert,
                ..
            }) => self.parse_revert(),
            // FORGET <hash> / FORGET SUBJECT — Tier-2 destruction, capped by
            // `allow_destructive_ops` and authorized by the session's
            // delete/erase grants at execution.
            Some(SpannedToken {
                token: Token::Forget,
                ..
            }) => self.parse_forget(),
            // PURGE OLDER THAN — the retention sweep (CAL 1.3 §8.14),
            // authorization-gated at execution like every Tier-2 statement.
            Some(SpannedToken {
                token: Token::Purge,
                ..
            }) => self.parse_purge(),
            // REPORT SUBJECT — the read-only DSAR selection (OMS 1.6
            // draft): the erasure selector in show-me mode.
            Some(SpannedToken {
                token: Token::Report,
                ..
            }) => self.parse_report_subject(),
            // Governance (CAL 1.3 §8.16): the loop lifecycle in the
            // language, BECAUSE as syntax.
            Some(SpannedToken {
                token: Token::Approve,
                ..
            }) => Ok(CalStatement::Approve(
                self.parse_governance_body(&Token::Approve)?,
            )),
            Some(SpannedToken {
                token: Token::Reject,
                ..
            }) => Ok(CalStatement::Reject(
                self.parse_governance_body(&Token::Reject)?,
            )),
            Some(SpannedToken {
                token: Token::Apply,
                ..
            }) => Ok(CalStatement::ApplyRec(
                self.parse_governance_body(&Token::Apply)?,
            )),
            Some(SpannedToken {
                token: Token::Rollback,
                ..
            }) => Ok(CalStatement::RollbackRec(
                self.parse_governance_body(&Token::Rollback)?,
            )),
            // Tier-3 DCL (CAL 1.3 §8.15).
            Some(SpannedToken {
                token: Token::Grant,
                ..
            }) => self.parse_grant(),
            Some(SpannedToken {
                token: Token::Revoke,
                ..
            }) => self.parse_revoke(),
            Some(SpannedToken {
                token: Token::Show,
                ..
            }) => self.parse_show_grants(),
            Some(SpannedToken {
                token: Token::Remember,
                ..
            }) => self.parse_remember(),
            // Wave-2 reads (CAL 1.3).
            Some(SpannedToken {
                token: Token::Entity,
                ..
            }) => self.parse_entity_at(),
            Some(SpannedToken {
                token: Token::Runs,
                ..
            }) => self.parse_runs_touching(),
            Some(SpannedToken {
                token: Token::Derived,
                ..
            }) => self.parse_derived_from(),
            Some(SpannedToken {
                token: Token::Merge,
                ..
            }) => self.parse_merge(),
            Some(SpannedToken {
                token: Token::Related,
                ..
            }) => self.parse_related(),
            Some(SpannedToken {
                token: Token::Novelty,
                ..
            }) => self.parse_novelty(),
            Some(SpannedToken {
                token: Token::Define,
                ..
            }) => self.parse_define(),
            Some(SpannedToken {
                token: Token::Drop, ..
            }) => self.parse_drop(),
            Some(SpannedToken {
                token: Token::Run, ..
            }) => self.parse_run_query(),
            Some(SpannedToken {
                token: Token::Stream,
                ..
            }) => self.parse_stream_assemble(),
            // Parenthesised statement (set operation).
            Some(SpannedToken {
                token: Token::LParen,
                ..
            }) => {
                let stmt = self.parse_paren_statement()?;
                self.parse_set_op_tail(stmt)
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "RECALL, EXISTS, ASSEMBLE, HISTORY, EXPLAIN, DESCRIBE, BATCH, \
                         COALESCE, ADD, SUPERSEDE, REVERT, FORGET, DEFINE, DROP, or RUN"
                        .into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::EmptyQuery { span: None }),
        }
    }

    /// Parse a statement wrapped in `( ... )`.
    fn parse_paren_statement(&mut self) -> CalResult<CalStatement> {
        self.expect_exact(&Token::LParen)?;
        self.enter_nesting()?;
        let stmt = self.parse_statement()?;
        self.leave_nesting();
        self.expect_exact(&Token::RParen)?;
        Ok(stmt)
    }

    /// Parse an ASSEMBLE labeled-source sub-query, accepting an optional
    /// trailing `WITH ...` INSIDE the parens. Returns the statement plus any
    /// inside-paren WITH options.
    ///
    /// Scoped to the assemble-source path only — `parse_paren_statement` is
    /// shared with the set-op operand path and must NOT change.
    fn parse_assemble_source_query(&mut self) -> CalResult<(CalStatement, Vec<WithOption>)> {
        if self.at_exact(&Token::LParen) {
            self.expect_exact(&Token::LParen)?;
            self.enter_nesting()?;
            // parse_statement already consumes any UNION/INTERSECT/EXCEPT set-op tail.
            let stmt = self.parse_statement()?;
            self.leave_nesting();
            // Optional inside-paren WITH (not WITH VARS).
            let inside_with = if self.at_exact(&Token::With) && !self.peek_next_is_vars() {
                self.parse_with_clause()?
            } else {
                vec![]
            };
            self.expect_exact(&Token::RParen)?;
            Ok((stmt, inside_with))
        } else {
            // Bare RECALL with no parens — no inside-paren WITH possible.
            let stmt = self.parse_recall_stmt()?;
            Ok((CalStatement::Recall(stmt), vec![]))
        }
    }

    /// Consume a UNION / INTERSECT / EXCEPT chain, if one follows.
    fn parse_set_op_tail(&mut self, first: CalStatement) -> CalResult<CalStatement> {
        let op = match self.peek() {
            Some(SpannedToken {
                token: Token::Union,
                ..
            }) => SetOp::Union,
            Some(SpannedToken {
                token: Token::Intersect,
                ..
            }) => SetOp::Intersect,
            Some(SpannedToken {
                token: Token::Except,
                ..
            }) => SetOp::Except,
            _ => return Ok(first),
        };

        let span_start = self.current_span();
        self.advance(); // consume UNION/INTERSECT/EXCEPT

        let mut operands = vec![first];
        // Parse the second operand (required).
        let rhs = if self.at_exact(&Token::LParen) {
            self.parse_paren_statement()?
        } else {
            let stmt = self.parse_recall_stmt()?;
            CalStatement::Recall(stmt)
        };
        operands.push(rhs);

        // Continue consuming if the same operator follows.
        while matches!(self.peek(), Some(st) if std::mem::discriminant(&st.token) == std::mem::discriminant(&Token::Union)
            || std::mem::discriminant(&st.token) == std::mem::discriminant(&Token::Intersect)
            || std::mem::discriminant(&st.token) == std::mem::discriminant(&Token::Except))
        {
            if operands.len() >= MAX_SET_OPERANDS {
                return Err(CalError::TooManySetOperands {
                    count: operands.len() + 1,
                    max: MAX_SET_OPERANDS,
                    span: Some(self.current_span()),
                });
            }
            self.advance(); // consume set-op keyword
            let next = if self.at_exact(&Token::LParen) {
                self.parse_paren_statement()?
            } else {
                let stmt = self.parse_recall_stmt()?;
                CalStatement::Recall(stmt)
            };
            operands.push(next);
        }

        let span_end = self.prev_span();
        Ok(CalStatement::SetOp(SetOpStmt {
            op,
            operands,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- RECALL -----------------------------------------------------------

    fn parse_recall_stmt(&mut self) -> CalResult<RecallStmt> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Recall)?;

        // Optional MY keyword (shorthand; emit a warning).
        let has_my = self.eat_exact(&Token::My);
        if has_my {
            self.warnings.push(CalWarning::UnknownRelation {
                relation: "MY (shorthand — will be desugared at execution time)".into(),
                span: Some(self.prev_span()),
            });
        }

        // Grain type is optional per OMS-1.4 EBNF (§4); when absent, only the
        // common field set is available in WHERE clauses (§6.2). Bug 2.
        // When no grain type is provided we fall back to the `All` wildcard,
        // which already represents "no type filter" downstream.
        let grain_type = if self.eat_exact(&Token::Asterisk) {
            // `RECALL *` — the explicit spelling of "any grain type"; the
            // same thing omitting the grain type already means.
            GrainTypePlural::All
        } else if self.at_exact(&Token::Where)
            || self.at_exact(&Token::Recent)
            || self.at_exact(&Token::Since)
            || self.at_exact(&Token::Until)
            || self.at_exact(&Token::Between)
            || self.at_exact(&Token::About)
            || self.at_exact(&Token::Like)
            || self.at_exact(&Token::Limit)
            || self.at_exact(&Token::Pipe)
        {
            GrainTypePlural::All
        } else {
            self.parse_grain_type_plural()?
        };

        // Optional clauses — order is flexible in the spec but we parse them
        // in a defined precedence to produce good error messages.

        let about = self.parse_about_clause()?;
        let like = self.parse_like_clause()?;
        let since = self.parse_since_clause()?;
        let until = self.parse_until_clause()?;
        let between = self.parse_between_clause()?;
        let mut where_clause = self.parse_where_clause()?;

        // If ABOUT was used without an explicit WHERE keyword but AND follows,
        // treat `AND condition` as an implicit WHERE clause continuation.
        // This handles: `RECALL facts ABOUT "topic" AND confidence >= 0.8 RECENT 10`
        if where_clause.is_none() && self.at_exact(&Token::And) {
            let wspan = self.current_span();
            self.advance(); // consume AND
            let cond = self.parse_condition_or()?;
            where_clause = Some(WhereClause {
                condition: cond,
                span: Some(wspan),
            });
        }

        // SINCE/UNTIL may appear after WHERE (e.g. `ABOUT "x" WHERE subject = "y" SINCE "2024-01-01"`).
        // Re-try parsing them if not already found before WHERE.
        let since = if since.is_none() {
            self.parse_since_clause()?
        } else {
            since
        };
        let until = if until.is_none() {
            self.parse_until_clause()?
        } else {
            until
        };

        let recent = self.parse_recent_clause()?;

        // Optional inline LIMIT (before pipeline).
        let limit = if self.at_exact(&Token::Limit) {
            self.advance();
            let span = self.current_span();
            let v = self.parse_u64()?;
            if v == 0 {
                return Err(CalError::LimitExceeded {
                    value: 0,
                    max: MAX_LIMIT_VALUE,
                    span: Some(span),
                });
            }
            if v > MAX_LIMIT_VALUE {
                return Err(CalError::LimitExceeded {
                    value: v,
                    max: MAX_LIMIT_VALUE,
                    span: Some(span),
                });
            }
            Some(v)
        } else {
            None
        };

        // Optional CONTRADICTIONS clause.
        let contradictions = self.parse_contradictions_clause()?;

        // Enforce spec §9.8 shortcut-combination matrix: ambiguous pairs
        // emit CAL-E060 instead of silently picking one interpretation.
        let where_refers_to = |field: &str| -> bool {
            // Iterative walk over the condition tree — explicit stack so a
            // long left-leaning AND/OR chain (bounded only by query bytes)
            // cannot exhaust the tokio worker stack.
            fn cond_refs(root: &Condition, f: &str) -> bool {
                let mut stack: Vec<&Condition> = vec![root];
                while let Some(c) = stack.pop() {
                    match c {
                        Condition::Comparison { field, .. }
                        | Condition::In { field, .. }
                        | Condition::NotIn { field, .. }
                            if field == f =>
                        {
                            return true;
                        }
                        Condition::And { left, right, .. } | Condition::Or { left, right, .. } => {
                            stack.push(left);
                            stack.push(right);
                        }
                        Condition::Not { inner, .. } => stack.push(inner),
                        _ => {}
                    }
                }
                false
            }
            where_clause
                .as_ref()
                .map(|w| cond_refs(&w.condition, field))
                .unwrap_or(false)
        };

        let report_conflict = |a: &str, b: &str, hint: &str| -> CalError {
            CalError::FieldNotOnGrainType {
                field: format!("{} + {}", a, b),
                grain_type: "RECALL".into(),
                span: Some(span_start),
                suggestion: Some(format!(
                    "Combination ambiguous per spec §9.8: {} with {}. {}",
                    a, b, hint
                )),
            }
        };

        if about.is_some() && like.is_some() {
            return Err(report_conflict(
                "ABOUT",
                "LIKE",
                "use ABOUT for semantic search or LIKE for textual similarity, not both.",
            ));
        }
        if recent.is_some() && limit.is_some() {
            return Err(report_conflict(
                "RECENT",
                "LIMIT",
                "RECENT N already caps the result count; remove LIMIT.",
            ));
        }
        if since.is_some() && where_refers_to("time") {
            return Err(report_conflict(
                "SINCE",
                "WHERE time",
                "SINCE is shorthand for WHERE time — pick one.",
            ));
        }
        if since.is_some() && between.is_some() {
            return Err(report_conflict(
                "SINCE",
                "BETWEEN",
                "SINCE sets a lower bound; BETWEEN gives an explicit range — pick one.",
            ));
        }
        if has_my && where_refers_to("user_id") {
            return Err(report_conflict(
                "MY",
                "WHERE user_id",
                "MY desugars to `WHERE user_id = $current_user_id` — remove the explicit clause.",
            ));
        }

        let span_end = self.prev_span();
        Ok(RecallStmt {
            grain_type,
            about,
            where_clause,
            recent,
            since,
            until,
            like,
            between,
            contradictions,
            limit,
            as_format: None,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        })
    }

    fn parse_about_clause(&mut self) -> CalResult<Option<AboutClause>> {
        if !self.at_exact(&Token::About) {
            return Ok(None);
        }
        let span = self.current_span();
        self.advance();
        let text = self.parse_string_literal()?;
        Ok(Some(AboutClause {
            text,
            span: Some(span),
        }))
    }

    fn parse_like_clause(&mut self) -> CalResult<Option<LikeClause>> {
        if !self.at_exact(&Token::Like) {
            return Ok(None);
        }
        let span = self.current_span();
        self.advance();
        let text = self.parse_string_literal()?;
        Ok(Some(LikeClause {
            text,
            span: Some(span),
        }))
    }

    fn parse_since_clause(&mut self) -> CalResult<Option<SinceClause>> {
        if !self.at_exact(&Token::Since) {
            return Ok(None);
        }
        let span = self.current_span();
        self.advance();
        let expression = self.parse_string_literal()?;
        Ok(Some(SinceClause {
            expression,
            span: Some(span),
        }))
    }

    fn parse_until_clause(&mut self) -> CalResult<Option<UntilClause>> {
        if !self.at_exact(&Token::Until) {
            return Ok(None);
        }
        let span = self.current_span();
        self.advance();
        let expression = self.parse_string_literal()?;
        Ok(Some(UntilClause {
            expression,
            span: Some(span),
        }))
    }

    fn parse_between_clause(&mut self) -> CalResult<Option<BetweenClause>> {
        if !self.at_exact(&Token::Between) {
            return Ok(None);
        }
        let span = self.current_span();
        self.advance();
        let start = self.parse_string_literal()?;
        self.expect_exact(&Token::And)?;
        let end = self.parse_string_literal()?;
        Ok(Some(BetweenClause {
            start,
            end,
            span: Some(span),
        }))
    }

    fn parse_recent_clause(&mut self) -> CalResult<Option<RecentClause>> {
        if !self.at_exact(&Token::Recent) {
            return Ok(None);
        }
        let span = self.current_span();
        self.advance();
        let count = self.parse_u64()?;
        Ok(Some(RecentClause {
            count,
            span: Some(span),
        }))
    }

    fn parse_contradictions_clause(&mut self) -> CalResult<Option<ContradictionsClause>> {
        if !self.at_exact(&Token::Contradictions) {
            return Ok(None);
        }
        let span = self.current_span();
        self.advance();
        // CONTRADICTIONS is a bare terminal per spec; the `OF (sub-query)`
        // tail is an Areev extension and is optional.
        self.eat_exact(&Token::Of);
        let inner = if self.at_exact(&Token::LParen) {
            Some(Box::new(self.parse_paren_statement()?))
        } else {
            None
        };
        Ok(Some(ContradictionsClause {
            inner,
            span: Some(span),
        }))
    }

    // -- WHERE ------------------------------------------------------------

    fn parse_where_clause(&mut self) -> CalResult<Option<WhereClause>> {
        if !self.at_exact(&Token::Where) {
            return Ok(None);
        }
        let span = self.current_span();
        self.advance();
        let condition = self.parse_condition_or()?;
        Ok(Some(WhereClause {
            condition,
            span: Some(span),
        }))
    }

    /// Parse an OR expression (lowest precedence inside WHERE).
    fn parse_condition_or(&mut self) -> CalResult<Condition> {
        let span_start = self.current_span();
        let mut left = self.parse_condition_and()?;
        while self.at_exact(&Token::Or) {
            self.advance();
            let right = self.parse_condition_and()?;
            let span_end = self.prev_span();
            left = Condition::Or {
                left: Box::new(left),
                right: Box::new(right),
                span: Some(Span::new(
                    span_start.start,
                    span_end.end,
                    span_start.line,
                    span_start.col,
                )),
            };
        }
        Ok(left)
    }

    /// Parse an AND expression.
    fn parse_condition_and(&mut self) -> CalResult<Condition> {
        let span_start = self.current_span();
        let mut left = self.parse_condition_unary()?;
        while self.at_exact(&Token::And) {
            self.advance();
            let right = self.parse_condition_unary()?;
            let span_end = self.prev_span();
            left = Condition::And {
                left: Box::new(left),
                right: Box::new(right),
                span: Some(Span::new(
                    span_start.start,
                    span_end.end,
                    span_start.line,
                    span_start.col,
                )),
            };
        }
        Ok(left)
    }

    /// Parse a NOT or primary condition.
    fn parse_condition_unary(&mut self) -> CalResult<Condition> {
        if self.at_exact(&Token::Not) {
            let span = self.current_span();
            self.advance();
            let inner = self.parse_condition_primary()?;
            return Ok(Condition::Not {
                inner: Box::new(inner),
                span: Some(span),
            });
        }
        self.parse_condition_primary()
    }

    /// Parse a primary condition (comparison, IN, IS NULL, CONTAINS, etc.)
    /// or a parenthesised sub-condition.
    fn parse_condition_primary(&mut self) -> CalResult<Condition> {
        // Parenthesised sub-condition.
        if self.at_exact(&Token::LParen) {
            self.advance();
            self.enter_nesting()?;
            let cond = self.parse_condition_or()?;
            self.leave_nesting();
            self.expect_exact(&Token::RParen)?;
            return Ok(cond);
        }

        let span_start = self.current_span();

        // Expect a field name (Ident token).
        let field = self.parse_field_name()?;

        // Determine the operator.
        match self.peek() {
            // `field = value` / `field != value` / `field >= value` / etc.
            Some(SpannedToken {
                token: Token::Eq | Token::NotEq | Token::Gte | Token::Lte | Token::Gt | Token::Lt,
                ..
            }) => {
                let st = self.advance().unwrap();
                let comparator = match &st.token {
                    Token::Eq => Comparator::Eq,
                    Token::NotEq => Comparator::NotEq,
                    Token::Gte => Comparator::Gte,
                    Token::Lte => Comparator::Lte,
                    Token::Gt => Comparator::Gt,
                    Token::Lt => Comparator::Lt,
                    _ => unreachable!(),
                };
                let value = self.parse_value()?;
                // Emit CAL-W001 when `relation = "mg:..."` references a
                // relation outside the standard vocabulary.
                if field == "relation" {
                    if let Value::String { value: ref s } = value {
                        if let Some(w) = super::relations::validate_relation(s) {
                            self.warnings.push(w);
                        }
                    }
                }
                let span_end = self.prev_span();
                Ok(Condition::Comparison {
                    field,
                    comparator,
                    value,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                })
            }

            // `field IN (v1, v2, ...)`
            Some(SpannedToken {
                token: Token::In, ..
            }) => {
                self.advance();
                // Allow `IN ($param)` where $param is an entire list.
                if self.at(&Token::Parameter("".into())) {
                    // Peek ahead: if next-after-param is `)`, it's a single param list.
                    let name = self.parse_parameter()?;
                    let values = vec![Value::Parameter { name }];
                    let span_end = self.prev_span();
                    return Ok(Condition::In {
                        field,
                        values,
                        span: Some(Span::new(
                            span_start.start,
                            span_end.end,
                            span_start.line,
                            span_start.col,
                        )),
                    });
                }
                let values = self.parse_value_list()?;
                let span_end = self.prev_span();
                Ok(Condition::In {
                    field,
                    values,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                })
            }

            // `field NOT IN (v1, v2, ...)`
            Some(SpannedToken {
                token: Token::Not, ..
            }) if self.peek_ahead(1).map(|t| &t.token) == Some(&Token::In) => {
                self.advance(); // NOT
                self.advance(); // IN
                let values = self.parse_value_list()?;
                let span_end = self.prev_span();
                Ok(Condition::NotIn {
                    field,
                    values,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                })
            }

            // `field IS NULL` / `field IS NOT NULL` / `field IS CATEGORY`
            Some(SpannedToken {
                token: Token::Is, ..
            }) => {
                self.advance(); // IS
                let not_null = self.eat_exact(&Token::Not);

                // Check for IS CATEGORY keywords (Preference, Knowledge, etc.)
                if !not_null {
                    if let Some(category) = self.try_parse_relation_category() {
                        let span_end = self.prev_span();
                        let span = Some(Span::new(
                            span_start.start,
                            span_end.end,
                            span_start.line,
                            span_start.col,
                        ));
                        return Ok(Condition::IsCategory {
                            field,
                            category,
                            span,
                        });
                    }
                }

                self.expect_exact(&Token::Null)?;
                let span_end = self.prev_span();
                let span = Some(Span::new(
                    span_start.start,
                    span_end.end,
                    span_start.line,
                    span_start.col,
                ));
                if not_null {
                    Ok(Condition::IsNotNull { field, span })
                } else {
                    Ok(Condition::IsNull { field, span })
                }
            }

            // `field INCLUDE [v, ...]` — desugar to `field IN [v, ...]` so
            // the executor's set-condition path handles it (mirrors EXCLUDE →
            // NotIn). Without this, single-element/array Comparison shapes
            // fall through to apply_where_clause's wildcard arm and emit a
            // spurious CAL-W010 warning even though the filter still works
            // via the type-specific extractor.
            Some(SpannedToken {
                token: Token::Include,
                ..
            }) => {
                self.advance();
                let include_span = self.current_span();
                let val = self.parse_value()?;
                // `tags INCLUDE` requires an array literal; reject scalars at parse time.
                let values = match val {
                    Value::Array { values } => values,
                    Value::Parameter { .. } => vec![val],
                    other => {
                        return Err(CalError::UnexpectedToken {
                            expected: "array literal `[...]` after INCLUDE".into(),
                            found: other.type_name().to_string(),
                            span: Some(include_span),
                            suggestion: Some(
                                "tags INCLUDE requires an array, e.g. tags INCLUDE [\"tag1\", \"tag2\"]".into(),
                            ),
                        });
                    }
                };
                let span_end = self.prev_span();
                Ok(Condition::In {
                    field,
                    values,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                })
            }

            // `field EXCLUDE [v, ...]` — tags exclude set (desugared to NOT IN).
            Some(SpannedToken {
                token: Token::Exclude,
                ..
            }) => {
                self.advance();
                let exclude_span = self.current_span();
                let val = self.parse_value()?;
                // Same array-literal type check as INCLUDE above.
                let values = match val {
                    Value::Array { values } => values,
                    Value::Parameter { .. } => vec![val],
                    other => {
                        return Err(CalError::UnexpectedToken {
                            expected: "array literal `[...]` after EXCLUDE".into(),
                            found: other.type_name().to_string(),
                            span: Some(exclude_span),
                            suggestion: Some(
                                "tags EXCLUDE requires an array, e.g. tags EXCLUDE [\"tag1\"]"
                                    .into(),
                            ),
                        });
                    }
                };
                let span_end = self.prev_span();
                Ok(Condition::NotIn {
                    field,
                    values,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                })
            }

            // `field CONTAINS "text"` — substring match.
            Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) if id.eq_ignore_ascii_case("CONTAINS") => {
                self.advance();
                let value = self.parse_string_literal()?;
                let span_end = self.prev_span();
                Ok(Condition::Contains {
                    field,
                    value,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                })
            }

            // `field STARTS WITH "text"` — prefix match.
            Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) if id.eq_ignore_ascii_case("STARTS") => {
                self.advance(); // consume STARTS
                                // Optionally consume WITH keyword (handles both Token::With and Token::Ident("WITH")).
                let has_with_keyword = self.at_exact(&Token::With)
                    || matches!(self.peek(), Some(SpannedToken { token: Token::Ident(id), .. }) if id.eq_ignore_ascii_case("WITH"));
                if has_with_keyword {
                    self.advance();
                }
                let value = self.parse_string_literal()?;
                let span_end = self.prev_span();
                Ok(Condition::StartsWith {
                    field,
                    value,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                })
            }

            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected:
                        "comparison operator (=, !=, >=, <=, >, <), IN, IS, CONTAINS, STARTS WITH, INCLUDE, or EXCLUDE"
                            .into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "comparison operator".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    /// Try to parse a relation category keyword (PREFERENCE, KNOWLEDGE, etc.)
    /// without consuming the token if it doesn't match. Returns the category
    /// string if matched.
    fn try_parse_relation_category(&mut self) -> Option<String> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Preference,
                ..
            }) => {
                self.advance();
                Some("preference".to_string())
            }
            Some(SpannedToken {
                token: Token::Knowledge,
                ..
            }) => {
                self.advance();
                Some("knowledge".to_string())
            }
            Some(SpannedToken {
                token: Token::Permission,
                ..
            }) => {
                self.advance();
                Some("permission".to_string())
            }
            Some(SpannedToken {
                token: Token::Interaction,
                ..
            }) => {
                self.advance();
                Some("interaction".to_string())
            }
            Some(SpannedToken {
                token: Token::Agency,
                ..
            }) => {
                self.advance();
                Some("agency".to_string())
            }
            Some(SpannedToken {
                token: Token::Lifecycle,
                ..
            }) => {
                self.advance();
                Some("lifecycle".to_string())
            }
            Some(SpannedToken {
                token: Token::Observation,
                ..
            }) => {
                self.advance();
                Some("observation".to_string())
            }
            // WORKFLOW and CONSENSUS are not dedicated keywords — they lex as Ident.
            Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) if id.eq_ignore_ascii_case("WORKFLOW") => {
                self.advance();
                Some("workflow".to_string())
            }
            Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) if id.eq_ignore_ascii_case("CONSENSUS") => {
                self.advance();
                Some("consensus".to_string())
            }
            Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) if id.eq_ignore_ascii_case("GOVERNANCE") => {
                self.advance();
                Some("governance".to_string())
            }
            _ => None,
        }
    }

    /// Parse a field name — either an `Ident` token or a dotted path.
    ///
    /// Returns the field as a string (e.g. `"subject"`, `"metadata.source"`).
    fn parse_field_name(&mut self) -> CalResult<String> {
        let first = self.parse_identifier()?;
        if self.at_exact(&Token::Dot) {
            self.advance();
            let second = self.parse_identifier()?;
            Ok(format!("{}.{}", first, second))
        } else {
            Ok(first)
        }
    }

    // -- PIPELINE ---------------------------------------------------------

    /// Check whether the current token starts a pipeline stage.
    fn at_pipeline_stage(&self) -> bool {
        if matches!(
            self.peek(),
            Some(SpannedToken {
                token: Token::Select
                    | Token::Order
                    | Token::Limit
                    | Token::Offset
                    | Token::Count
                    | Token::First
                    | Token::Subjects
                    | Token::Objects
                    | Token::Hashes
                    | Token::Group
                    | Token::Project,
                ..
            })
        ) {
            return true;
        }
        // SORT is an alias for ORDER BY — treat as pipeline stage.
        matches!(
            self.peek(),
            Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) if id.eq_ignore_ascii_case("SORT")
        )
    }

    fn parse_pipeline(&mut self) -> CalResult<Vec<PipelineStage>> {
        let mut stages = vec![];
        // Accept pipeline stages with or without leading `|`.
        while self.at_exact(&Token::Pipe) || self.at_pipeline_stage() {
            if stages.len() >= MAX_PIPELINE_STAGES {
                return Err(CalError::TooManyPipelineStages {
                    count: stages.len() + 1,
                    max: MAX_PIPELINE_STAGES,
                    span: Some(self.current_span()),
                });
            }
            // Consume optional `|` (backward compatibility) but warn.
            if self.eat_exact(&Token::Pipe) {
                self.warnings.push(CalWarning::DeprecatedPipeOperator {
                    span: Some(self.current_span()),
                });
            }
            let stage = self.parse_pipeline_stage()?;
            stages.push(stage);
        }
        Ok(stages)
    }

    fn parse_pipeline_stage(&mut self) -> CalResult<PipelineStage> {
        let span = self.current_span();
        match self.peek() {
            Some(SpannedToken {
                token: Token::Select,
                ..
            }) => {
                self.advance();
                let mut fields = vec![];
                loop {
                    fields.push(self.parse_identifier()?);
                    if !self.eat_exact(&Token::Comma) {
                        break;
                    }
                }
                Ok(PipelineStage::Select {
                    fields,
                    span: Some(span),
                })
            }
            Some(SpannedToken {
                token: Token::Order,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::By)?;
                let field = self.parse_identifier()?;
                let descending = if self.at_exact(&Token::Desc) {
                    self.advance();
                    true
                } else {
                    self.eat_exact(&Token::Asc);
                    false
                };
                Ok(PipelineStage::OrderBy {
                    field,
                    descending,
                    span: Some(span),
                })
            }
            Some(SpannedToken {
                token: Token::Limit,
                ..
            }) => {
                self.advance();
                let lspan = self.current_span();
                let value = self.parse_u64()?;
                if value == 0 {
                    return Err(CalError::LimitExceeded {
                        value: 0,
                        max: MAX_LIMIT_VALUE,
                        span: Some(lspan),
                    });
                }
                if value > MAX_LIMIT_VALUE {
                    return Err(CalError::LimitExceeded {
                        value,
                        max: MAX_LIMIT_VALUE,
                        span: Some(lspan),
                    });
                }
                Ok(PipelineStage::Limit {
                    value,
                    span: Some(span),
                })
            }
            Some(SpannedToken {
                token: Token::Offset,
                ..
            }) => {
                self.advance();
                let value = self.parse_u64()?;
                Ok(PipelineStage::Offset {
                    value,
                    span: Some(span),
                })
            }
            Some(SpannedToken {
                token: Token::Count,
                ..
            }) => {
                self.advance();
                Ok(PipelineStage::Count { span: Some(span) })
            }
            Some(SpannedToken {
                token: Token::First,
                ..
            }) => {
                self.advance();
                Ok(PipelineStage::First { span: Some(span) })
            }
            Some(SpannedToken {
                token: Token::Subjects,
                ..
            }) => {
                self.advance();
                Ok(PipelineStage::Subjects { span: Some(span) })
            }
            Some(SpannedToken {
                token: Token::Objects,
                ..
            }) => {
                self.advance();
                Ok(PipelineStage::Objects { span: Some(span) })
            }
            Some(SpannedToken {
                token: Token::Hashes,
                ..
            }) => {
                self.advance();
                Ok(PipelineStage::Hashes { span: Some(span) })
            }
            Some(SpannedToken {
                token: Token::Group,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::By)?;
                let field = self.parse_identifier()?;
                Ok(PipelineStage::GroupBy {
                    field,
                    span: Some(span),
                })
            }
            Some(SpannedToken {
                token: Token::Project,
                ..
            }) => {
                self.advance();
                let mut fields = vec![];
                loop {
                    let field = self.parse_identifier()?;
                    let alias = if self.eat_exact(&Token::As) {
                        Some(self.parse_identifier()?)
                    } else {
                        None
                    };
                    fields.push(ProjectField { field, alias });
                    if !self.eat_exact(&Token::Comma) {
                        break;
                    }
                }
                Ok(PipelineStage::Project {
                    fields,
                    span: Some(span),
                })
            }
            // `SORT field [ASC|DESC]` — alias for ORDER BY.
            Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) if id.eq_ignore_ascii_case("SORT") => {
                self.advance(); // consume SORT
                let field = self.parse_identifier()?;
                let descending = if self.at_exact(&Token::Desc) {
                    self.advance();
                    true
                } else {
                    self.eat_exact(&Token::Asc);
                    false
                };
                Ok(PipelineStage::OrderBy {
                    field,
                    descending,
                    span: Some(span),
                })
            }
            Some(st) => {
                let found = st.token.description();
                let sp = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "pipeline stage (SELECT, ORDER BY, LIMIT, OFFSET, COUNT, FIRST, \
                         SUBJECTS, OBJECTS, HASHES, GROUP BY, PROJECT)"
                        .into(),
                    found,
                    span: Some(sp),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "pipeline stage".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    // -- WITH clause ------------------------------------------------------

    fn parse_with_clause(&mut self) -> CalResult<Vec<WithOption>> {
        self.expect_exact(&Token::With)?;
        let mut options = vec![];
        loop {
            // I-6 fix: parse_with_option returns None for unknown options
            // (warning already emitted); we skip without pushing.
            if let Some(opt) = self.parse_with_option()? {
                options.push(opt);
            }
            if !self.eat_exact(&Token::Comma) {
                break;
            }
            // Allow trailing comma: if the next token is clearly not a
            // WITH option candidate, break.  We now accept identifiers
            // as potential unknown options (I-6: warn and skip).
            match self.peek() {
                None => break,
                Some(SpannedToken {
                    token: Token::Pipe, ..
                }) => break,
                Some(SpannedToken {
                    token: Token::Format,
                    ..
                }) => break,
                _ => {
                    // Continue parsing — could be a known or unknown option.
                }
            }
        }
        Ok(options)
    }

    /// Parse a single WITH option.
    ///
    /// Returns `Ok(Some(option))` for recognized options, `Ok(None)` for
    /// unknown options (I-6 fix: warns and skips without pushing a
    /// placeholder), and `Err` for end-of-input.
    fn parse_with_option(&mut self) -> CalResult<Option<WithOption>> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Superseded,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::Superseded))
            }
            Some(SpannedToken {
                token: Token::ScoreBreakdown,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::ScoreBreakdown))
            }
            Some(SpannedToken {
                token: Token::Explanation,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::Explanation))
            }
            Some(SpannedToken {
                token: Token::Provenance,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::Provenance))
            }
            Some(SpannedToken {
                token: Token::ContradictionDetection,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::ContradictionDetection))
            }
            Some(SpannedToken {
                token: Token::Diversity,
                ..
            }) => {
                self.advance();
                let lambda = if self.at_exact(&Token::LParen) {
                    self.advance();
                    let n = self.parse_number()?;
                    self.expect_exact(&Token::RParen)?;
                    Some(n)
                } else {
                    None
                };
                Ok(Some(WithOption::Diversity { lambda }))
            }
            Some(SpannedToken {
                token: Token::Dedup,
                ..
            }) => {
                self.advance();
                // EBNF: `"dedup" , "(" , field_name , ")"` — argument is a field name.
                let field = if self.at_exact(&Token::LParen) {
                    self.advance();
                    let f = self.parse_field_name()?;
                    self.expect_exact(&Token::RParen)?;
                    Some(f)
                } else {
                    None
                };
                Ok(Some(WithOption::Dedup { field }))
            }
            // OMS §4 `progressive_disclosure` / `progressive_disclosure(level)`.
            Some(SpannedToken {
                token: Token::ProgressiveDisclosure,
                span,
                ..
            }) => {
                let span = *span;
                self.advance();
                let level = if self.at_exact(&Token::LParen) {
                    self.advance();
                    let lvl = self.parse_identifier()?;
                    let lvl_lc = lvl.to_ascii_lowercase();
                    if !matches!(lvl_lc.as_str(), "summary" | "headlines" | "full") {
                        return Err(CalError::UnexpectedToken {
                            expected: "one of: summary, headlines, full".into(),
                            found: lvl,
                            span: Some(self.prev_span()),
                            suggestion: Some(
                                "progressive_disclosure level must be summary | headlines | full (spec §4).".into(),
                            ),
                        });
                    }
                    self.expect_exact(&Token::RParen)?;
                    Some(lvl_lc)
                } else {
                    None
                };
                let _ = span;
                Ok(Some(WithOption::ProgressiveDisclosure { level }))
            }
            // OMS §4 `consistency(level)` where level ∈ eventual|bounded|linearizable.
            Some(SpannedToken {
                token: Token::Consistency,
                span,
                ..
            }) => {
                let span = *span;
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let lvl = self.parse_identifier()?;
                let lvl_lc = lvl.to_ascii_lowercase();
                if !matches!(lvl_lc.as_str(), "eventual" | "bounded" | "linearizable") {
                    return Err(CalError::UnexpectedToken {
                        expected: "one of: eventual, bounded, linearizable".into(),
                        found: lvl,
                        span: Some(self.prev_span()),
                        suggestion: Some(
                            "consistency level must be eventual | bounded | linearizable (spec §4).".into(),
                        ),
                    });
                }
                self.expect_exact(&Token::RParen)?;
                self.warnings.push(CalWarning::UnknownExtensionOption {
                    option: "consistency (parsed but executor no-op)".into(),
                    span: Some(span),
                });
                Ok(Some(WithOption::Consistency {
                    level: Some(lvl_lc),
                }))
            }
            // OMS §4 `locale("en-US")`.
            Some(SpannedToken {
                token: Token::Locale,
                span,
                ..
            }) => {
                let span = *span;
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let tag = self.parse_string_literal()?;
                self.expect_exact(&Token::RParen)?;
                self.warnings.push(CalWarning::UnknownExtensionOption {
                    option: "locale (parsed but executor no-op)".into(),
                    span: Some(span),
                });
                Ok(Some(WithOption::Locale { tag }))
            }
            // OMS §4 `cache(ttl=300)`.
            Some(SpannedToken {
                token: Token::Cache,
                span,
                ..
            }) => {
                let span = *span;
                self.advance();
                self.expect_exact(&Token::LParen)?;
                // Accept either `ttl=N` (spec) or a bare positive integer.
                if self.at_exact(&Token::Ttl) {
                    self.advance();
                    self.expect_exact(&Token::Eq)?;
                }
                let ttl_seconds = self.parse_u64()?;
                self.expect_exact(&Token::RParen)?;
                self.warnings.push(CalWarning::UnknownExtensionOption {
                    option: "cache (parsed but executor no-op)".into(),
                    span: Some(span),
                });
                Ok(Some(WithOption::Cache { ttl_seconds }))
            }
            // -- Recall feature flags (parity with HTTP/gRPC/MCP/A2A) --------
            Some(SpannedToken {
                token: Token::Rerank,
                ..
            }) => {
                self.advance();
                let model = if self.at_exact(&Token::LParen) {
                    self.advance();
                    let m = self.parse_string_literal()?;
                    self.expect_exact(&Token::RParen)?;
                    Some(m)
                } else {
                    None
                };
                Ok(Some(WithOption::Rerank { model }))
            }
            Some(SpannedToken {
                token: Token::LlmRerank,
                ..
            }) => {
                self.advance();
                let model = if self.at_exact(&Token::LParen) {
                    self.advance();
                    let m = self.parse_string_literal()?;
                    self.expect_exact(&Token::RParen)?;
                    Some(m)
                } else {
                    None
                };
                Ok(Some(WithOption::LlmRerank { model }))
            }
            Some(SpannedToken {
                token: Token::QueryExpansion,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::QueryExpansion))
            }
            Some(SpannedToken {
                token: Token::QueryDecompose,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::QueryDecompose))
            }
            Some(SpannedToken {
                token: Token::Hyde, ..
            }) => {
                self.advance();
                Ok(Some(WithOption::Hyde))
            }
            Some(SpannedToken {
                token: Token::ConflictResolution,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::ConflictResolution))
            }
            Some(SpannedToken {
                token: Token::IncludeSources,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::IncludeSources))
            }
            Some(SpannedToken {
                token: Token::AnnotateRelativeTime,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::AnnotateRelativeTime))
            }
            Some(SpannedToken {
                token: Token::RecencyWeight,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let weight = self.parse_number()?;
                self.expect_exact(&Token::RParen)?;
                Ok(Some(WithOption::RecencyWeight { weight }))
            }
            Some(SpannedToken {
                token: Token::MinScore,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let score = self.parse_number()?;
                self.expect_exact(&Token::RParen)?;
                Ok(Some(WithOption::MinScore { score }))
            }
            Some(SpannedToken {
                token: Token::MultiHop,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let hops = self.parse_u64()?;
                self.expect_exact(&Token::RParen)?;
                Ok(Some(WithOption::MultiHop { hops }))
            }

            Some(SpannedToken {
                token: Token::SessionAffinity,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let boost = self.parse_number()?;
                self.expect_exact(&Token::RParen)?;
                Ok(Some(WithOption::SessionAffinity { boost }))
            }
            Some(SpannedToken {
                token: Token::SubjectAffinity,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let boost = self.parse_number()?;
                self.expect_exact(&Token::RParen)?;
                Ok(Some(WithOption::SubjectAffinity { boost }))
            }
            Some(SpannedToken {
                token: Token::SessionCoverage,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let min_per_ns = self.parse_u64()?;
                self.expect_exact(&Token::RParen)?;
                Ok(Some(WithOption::SessionCoverage { min_per_ns }))
            }
            Some(SpannedToken {
                token: Token::MaxNamespaces,
                ..
            }) => {
                self.advance();
                self.expect_exact(&Token::LParen)?;
                let max = self.parse_u64()?;
                self.expect_exact(&Token::RParen)?;
                Ok(Some(WithOption::MaxNamespaces { max }))
            }
            Some(SpannedToken {
                token: Token::Exhaustive,
                ..
            }) => {
                self.advance();
                // Optional parameter: `WITH exhaustive` or `WITH exhaustive(3)`
                let max_rounds = if self.peek().map(|st| &st.token) == Some(&Token::LParen) {
                    self.expect_exact(&Token::LParen)?;
                    let rounds = self.parse_u64()?;
                    self.expect_exact(&Token::RParen)?;
                    Some(rounds)
                } else {
                    None
                };
                Ok(Some(WithOption::Exhaustive { max_rounds }))
            }
            Some(SpannedToken {
                token: Token::SessionCensus,
                ..
            }) => {
                self.advance();
                // Optional positional parameters: `WITH session_census` or
                // `WITH session_census(2)` or `WITH session_census(2, 0.35)`
                let (mut min_per_session, mut min_score) = (None, None);
                if self.peek().map(|st| &st.token) == Some(&Token::LParen) {
                    self.expect_exact(&Token::LParen)?;
                    min_per_session = Some(self.parse_u64()?);
                    if self.peek().map(|st| &st.token) == Some(&Token::Comma) {
                        self.advance();
                        min_score = Some(self.parse_number()?);
                    }
                    self.expect_exact(&Token::RParen)?;
                }
                Ok(Some(WithOption::SessionCensus {
                    min_per_session,
                    min_score,
                }))
            }
            Some(SpannedToken {
                token: Token::AggregationIntent,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::AggregationIntent))
            }

            Some(SpannedToken {
                token: Token::PreferenceEnrichment,
                ..
            }) => {
                self.advance();
                Ok(Some(WithOption::PreferenceEnrichment))
            }

            // -- Unknown option fallback ----------------------------------
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                self.warnings.push(CalWarning::UnknownExtensionOption {
                    option: found,
                    span: Some(span),
                });
                // I-6 fix: consume the unknown token so parsing can
                // continue, but return None so the caller does NOT push
                // a placeholder Superseded variant into the options list.
                self.advance();
                Ok(None)
            }
            None => Err(CalError::UnexpectedToken {
                expected: "WITH option".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    // -- FORMAT -----------------------------------------------------------

    /// Maximum number of formats in a multi-format list (CAL-E110).
    const MAX_MULTI_FORMATS: usize = 5;

    fn parse_format(&mut self) -> CalResult<FormatClause> {
        self.expect_exact(&Token::Format)?;
        self.parse_format_value()
    }

    /// Parse `AS <format>` — §7 `as_clause`, per-query format control.
    ///
    /// Same value grammar as `FORMAT`, including bracketed multi-format
    /// lists. `AS OF` belongs to HISTORY and is matched before this.
    fn parse_as_format(&mut self) -> CalResult<FormatClause> {
        self.expect_exact(&Token::As)?;
        self.parse_format_value()
    }

    fn parse_format_value(&mut self) -> CalResult<FormatClause> {
        // Check for multi-format list: FORMAT [json, markdown, ...]
        if self.at_exact(&Token::LBracket) {
            return self.parse_format_list();
        }

        // Parse first format spec.
        let spec = self.parse_single_format_spec()?;

        // Check for comma-separated multi-format: FORMAT json, markdown
        // (aliases are only supported in bracketed lists)
        if self.at_exact(&Token::Comma) {
            let mut entries = vec![AliasedFormat { spec, alias: None }];
            while self.eat_exact(&Token::Comma) {
                let next = self.parse_single_format_spec()?;
                let entry = AliasedFormat {
                    spec: next,
                    alias: None,
                };
                if !entries.iter().any(|e| e.spec == entry.spec) {
                    entries.push(entry);
                }
            }
            if entries.len() > Self::MAX_MULTI_FORMATS {
                return Err(CalError::TooManyFormats {
                    count: entries.len(),
                    max: Self::MAX_MULTI_FORMATS,
                    span: Some(self.current_span()),
                });
            }
            return Ok(FormatClause::Multi(entries));
        }

        Ok(FormatClause::Single(spec))
    }

    /// Parse a bracketed multi-format list: `[json AS alias, markdown, ...]`.
    /// The opening `[` has been detected but not consumed.
    fn parse_format_list(&mut self) -> CalResult<FormatClause> {
        let span_start = self.current_span();
        self.expect_exact(&Token::LBracket)?;

        // Empty list is a parse error.
        if self.at_exact(&Token::RBracket) {
            return Err(CalError::UnexpectedToken {
                expected: "at least one format type in format list".into(),
                found: "]".into(),
                span: Some(span_start),
                suggestion: Some("FORMAT [json] or FORMAT [markdown, json]".into()),
            });
        }

        let mut entries = Vec::new();
        let mut seen_keys = std::collections::HashSet::new();
        loop {
            let spec = self.parse_single_format_spec()?;

            // Optional alias: `AS <name>`. A user-chosen output name is a
            // label, not an identifier — several perfectly ordinary alias
            // words (DATA, READABLE, COMPACT) are keyword tokens, so
            // `parse_identifier` would reject the reference manual's own
            // example `FORMAT [json AS data, markdown AS readable]`.
            let alias = if self.eat_exact(&Token::As) {
                let span = self.current_span();
                let name = self.parse_label()?;
                // Defence in depth (invariant 3): an alias is only a key in the
                // output map and cannot destroy anything, but the destructive
                // vocabulary stays out of CAL text everywhere it can appear.
                if is_destructive_keyword(&name) {
                    return Err(CalError::UnexpectedToken {
                        expected: "an output name".into(),
                        found: name,
                        span: Some(span),
                        suggestion: Some(
                            "Choose a different alias — CAL keeps destructive \
                             words out of its grammar entirely."
                                .into(),
                        ),
                    });
                }
                Some(name)
            } else {
                None
            };

            // Determine the effective key for dedup/collision detection.
            let key = alias.as_deref().unwrap_or(spec.canonical_key()).to_string();
            if !seen_keys.insert(key.clone()) {
                // If an explicit alias collides, that's an error (CAL-E113).
                // If no alias was given, silently deduplicate (backward compat).
                if alias.is_some() {
                    return Err(CalError::DuplicateFormatKey {
                        key,
                        span: Some(self.current_span()),
                    });
                }
                // Skip duplicate non-aliased format.
            } else {
                entries.push(AliasedFormat { spec, alias });
            }

            if self.eat_exact(&Token::Comma) {
                continue;
            }
            break;
        }

        self.expect_exact(&Token::RBracket)?;

        // Validate max count (CAL-E110).
        if entries.len() > Self::MAX_MULTI_FORMATS {
            return Err(CalError::TooManyFormats {
                count: entries.len(),
                max: Self::MAX_MULTI_FORMATS,
                span: Some(span_start),
            });
        }

        Ok(FormatClause::Multi(entries))
    }

    /// Parse a single format spec (json, markdown, sml, etc.).
    fn parse_single_format_spec(&mut self) -> CalResult<FormatSpec> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Json, ..
            }) => {
                self.advance();
                Ok(FormatSpec::Json)
            }
            Some(SpannedToken {
                token: Token::Yaml, ..
            }) => {
                self.advance();
                Ok(FormatSpec::Yaml)
            }
            Some(SpannedToken {
                token: Token::Markdown,
                ..
            }) => {
                self.advance();
                Ok(FormatSpec::Markdown)
            }
            Some(SpannedToken {
                token: Token::Text, ..
            }) => {
                self.advance();
                Ok(FormatSpec::Text)
            }
            Some(SpannedToken {
                token: Token::Sml, ..
            }) => {
                self.advance();
                Ok(FormatSpec::Sml)
            }
            Some(SpannedToken {
                token: Token::Toon, ..
            }) => {
                self.advance();
                Ok(FormatSpec::Toon)
            }
            Some(SpannedToken {
                token: Token::Triples,
                ..
            }) => {
                self.advance();
                Ok(FormatSpec::Triples)
            }
            // §10.1 semantic presets. These are aliases, not distinct
            // renderers: `structured`/`sml`, `readable`/`markdown`,
            // `compact`/`text`, `data`/`json`.
            Some(SpannedToken {
                token: Token::Structured,
                ..
            }) => {
                self.advance();
                Ok(FormatSpec::Sml)
            }
            Some(SpannedToken {
                token: Token::Readable,
                ..
            }) => {
                self.advance();
                Ok(FormatSpec::Markdown)
            }
            Some(SpannedToken {
                token: Token::Compact,
                ..
            }) => {
                self.advance();
                Ok(FormatSpec::Text)
            }
            Some(SpannedToken {
                token: Token::Data, ..
            }) => {
                self.advance();
                Ok(FormatSpec::Json)
            }
            Some(SpannedToken {
                token: Token::Template,
                ..
            }) => {
                self.advance();
                // §10.6 / §10.6.1 — three forms, told apart by token class so
                // no lookahead is needed:
                //   TEMPLATE <ident>   a registered template
                //   TEMPLATE "<text>"  an inline ELEMENT shorthand
                //   TEMPLATE { ... }   inline sections
                match self.peek() {
                    // A name here is whatever `DEFINE TEMPLATE` accepted as
                    // one, which is `parse_label`, not `parse_identifier`:
                    // preset names (`structured`, `compact`, …) are keyword
                    // tokens. Matching only `Ident` made a template nameable
                    // but not referenceable.
                    Some(st) if Self::is_word_token(&st.token) => {
                        Ok(FormatSpec::TemplateRef {
                            name: self.parse_template_name()?,
                        })
                    }
                    Some(SpannedToken {
                        token: Token::LBrace,
                        ..
                    }) => {
                        self.advance();
                        let sections = self.parse_template_sections()?;
                        self.expect_exact(&Token::RBrace)?;
                        if sections.is_empty() {
                            return Err(CalError::TemplateSyntaxError {
                                detail: "inline TEMPLATE must define at least one section"
                                    .to_string(),
                                span: Some(self.current_span()),
                            });
                        }
                        Ok(FormatSpec::TemplateInline { sections })
                    }
                    _ => {
                        let template = self.parse_string_literal()?;
                        Ok(FormatSpec::Template { template })
                    }
                }
            }
            Some(SpannedToken {
                token: Token::Ident(name),
                ..
            }) if name.eq_ignore_ascii_case("preset") => {
                self.advance();
                // Parse preset("template_name") or preset "template_name".
                let preset_name = if self.at_exact(&Token::LParen) {
                    self.advance(); // consume (
                    let n = self.parse_string_literal()?;
                    self.expect_exact(&Token::RParen)?;
                    n
                } else {
                    self.parse_string_literal()?
                };
                Ok(FormatSpec::Preset { name: preset_name })
            }
            Some(SpannedToken {
                token: Token::Ident(name),
                ..
            }) if name.eq_ignore_ascii_case("csv") => {
                self.advance();
                Ok(FormatSpec::Csv)
            }
            Some(SpannedToken {
                token: Token::Ident(name),
                ..
            }) if name.eq_ignore_ascii_case("table") => {
                self.advance();
                Ok(FormatSpec::Table)
            }
            Some(SpannedToken {
                token: Token::Ident(name),
                ..
            }) => {
                let name = name.clone();
                let span = self.current_span();
                Err(CalError::UnexpectedToken {
                    expected: "format type (json, yaml, markdown, text, sml, toon, triples, csv, table, template, or preset)".into(),
                    found: name,
                    span: Some(span),
                    suggestion: Some("Valid formats: json, yaml, markdown, text, sml, toon, triples, csv, table, template \"...\", preset(\"...\")".into()),
                })
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected: "format type (json, yaml, markdown, text, sml, toon, triples, or preset name)".into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "format type".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    // -- WITH VARS --------------------------------------------------------

    /// Maximum number of user variables in a `WITH VARS` clause.
    const MAX_USER_VARS: usize = 10;

    /// Maximum size in bytes of a single user variable value.
    const MAX_USER_VAR_SIZE: usize = 1024;

    /// Peek ahead to check if the token after `WITH` is `VARS`.
    ///
    /// Does NOT consume any tokens. Returns `false` if the current token
    /// is not `WITH` or if the next token is not `VARS`.
    /// True when the next token after `AS` is `OF` — the HISTORY temporal
    /// clause, which must not be mistaken for per-query format control.
    fn peek_next_is_of(&self) -> bool {
        matches!(
            self.tokens.get(self.pos + 1),
            Some(SpannedToken {
                token: Token::Of,
                ..
            })
        )
    }

    fn peek_next_is_vars(&self) -> bool {
        if !self.at_exact(&Token::With) {
            return false;
        }
        // Look at the token after WITH.
        matches!(
            self.tokens.get(self.pos + 1),
            Some(SpannedToken {
                token: Token::Vars,
                ..
            })
        )
    }

    /// Parse `WITH VARS { "key": "value", ... }`.
    ///
    /// The caller has verified that the current token is `WITH` and the
    /// next is `VARS` via `peek_next_is_vars()`.
    fn parse_user_vars(&mut self) -> CalResult<HashMap<String, String>> {
        let span_start = self.current_span();
        self.expect_exact(&Token::With)?;
        self.expect_exact(&Token::Vars)?;
        self.expect_exact(&Token::LBrace)?;

        let mut vars = HashMap::new();

        // Empty braces: WITH VARS { }
        if self.at_exact(&Token::RBrace) {
            self.advance();
            return Ok(vars);
        }

        loop {
            // Key: must be a string literal.
            let key = self.parse_string_literal()?;

            // Validate key is a valid identifier: [a-zA-Z_][a-zA-Z0-9_]*
            if key.is_empty()
                || (!key.as_bytes()[0].is_ascii_alphabetic() && key.as_bytes()[0] != b'_')
            {
                return Err(CalError::UnexpectedToken {
                    expected: "valid variable name (must start with a letter or underscore)".into(),
                    found: key,
                    span: Some(span_start),
                    suggestion: Some("variable names must match [a-zA-Z_][a-zA-Z0-9_]*".into()),
                });
            }
            if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(CalError::UnexpectedToken {
                    expected: "valid variable name (alphanumeric and underscores only)".into(),
                    found: key,
                    span: Some(span_start),
                    suggestion: Some("variable names must match [a-zA-Z_][a-zA-Z0-9_]*".into()),
                });
            }

            // Colon separator.
            self.expect_exact(&Token::Colon)?;

            // Value: must be a string literal.
            let value = self.parse_string_literal()?;

            // Validate value size (max 1KB).
            if value.len() > Self::MAX_USER_VAR_SIZE {
                return Err(CalError::UserVarTooLarge {
                    key,
                    size: value.len(),
                    max: Self::MAX_USER_VAR_SIZE,
                    span: Some(span_start),
                });
            }

            vars.insert(key, value);

            // Check max count.
            if vars.len() > Self::MAX_USER_VARS {
                return Err(CalError::TooManyUserVars {
                    count: vars.len(),
                    max: Self::MAX_USER_VARS,
                    span: Some(span_start),
                });
            }

            // Comma or closing brace.
            if !self.eat_exact(&Token::Comma) {
                break;
            }
            // Allow trailing comma.
            if self.at_exact(&Token::RBrace) {
                break;
            }
        }

        self.expect_exact(&Token::RBrace)?;

        Ok(vars)
    }

    // -- ASSEMBLE ---------------------------------------------------------

    fn parse_assemble(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Assemble)?;

        // Optional topic/context name. EBNF `context_name = identifier`, so
        // both bare identifiers and quoted strings are accepted.
        let topic = if self.at(&Token::StringLiteral("".into())) {
            self.parse_string_literal()?
        } else if let Some(SpannedToken {
            token: Token::Ident(word),
            ..
        }) = self.peek()
        {
            let word = word.clone();
            self.advance();
            word
        } else {
            String::new()
        };
        // Per OMS §8.2 ASSEMBLE constraints: max context_name length 64.
        const MAX_CONTEXT_NAME_LEN: usize = 64;
        if topic.chars().count() > MAX_CONTEXT_NAME_LEN {
            return Err(CalError::UnexpectedToken {
                expected: format!("context name ≤ {} characters", MAX_CONTEXT_NAME_LEN),
                found: format!("{}-character context name", topic.chars().count()),
                span: Some(span_start),
                suggestion: Some(format!(
                    "Shorten the ASSEMBLE context name (spec §8.2: max {} chars).",
                    MAX_CONTEXT_NAME_LEN
                )),
            });
        }
        let context_name = if !topic.is_empty() {
            Some(topic.clone())
        } else {
            None
        };

        // Optional `FOR "..."` — bounded at 256 chars per OMS §8.2.
        const MAX_FOR_LEN: usize = 256;
        let for_whom = if self.at_exact(&Token::For) {
            self.advance();
            let s = self.parse_string_literal()?;
            if s.chars().count() > MAX_FOR_LEN {
                return Err(CalError::UnexpectedToken {
                    expected: format!("FOR string ≤ {} characters", MAX_FOR_LEN),
                    found: format!("{}-character FOR clause", s.chars().count()),
                    span: Some(span_start),
                    suggestion: Some(format!(
                        "Shorten the ASSEMBLE FOR string (spec §8.2: max {} chars).",
                        MAX_FOR_LEN
                    )),
                });
            }
            Some(s)
        } else {
            None
        };

        // `FROM source` — try multi-source first, fall back to single source.
        self.expect_exact(&Token::From)?;
        let (from, sources) = self.parse_assemble_from_clause()?;

        // Issue 2: validate multi-source constraints (CAL-E032, CAL-E034).
        if let Some(ref srcs) = sources {
            // CAL-E032: Too many sources (max 8).
            if srcs.len() > 8 {
                return Err(CalError::AssembleTooManySources {
                    count: srcs.len(),
                    max: 8,
                    span: Some(span_start),
                });
            }
            // CAL-E034: Duplicate source labels.
            let mut seen_labels = std::collections::HashSet::new();
            for src in srcs {
                if !seen_labels.insert(&src.label) {
                    return Err(CalError::AssembleDuplicateLabel {
                        label: src.label.clone(),
                        span: src.span,
                    });
                }
            }
        }

        // Optional WHERE.
        let where_clause = self.parse_where_clause()?;

        // Issue 6: WHERE is not supported with multi-source ASSEMBLE.
        if sources.is_some() && where_clause.is_some() {
            return Err(CalError::UnexpectedToken {
                expected: "BUDGET, PRIORITY, FORMAT, or WITH".into(),
                found: "WHERE".into(),
                span: where_clause.as_ref().and_then(|wc| wc.span),
                suggestion: Some(
                    "WHERE is not supported with multi-source ASSEMBLE — apply WHERE inside each source's RECALL instead".into(),
                ),
            });
        }

        // Optional BUDGET clause.
        let budget = if self.at_exact(&Token::Budget) {
            self.advance();
            let bspan = self.current_span();
            let raw = self.parse_u64()?;
            // Issue 7: range check before u64→u32 cast.
            if raw > u32::MAX as u64 {
                return Err(CalError::AssembleBudgetExceeded {
                    value: raw,
                    max: 16000,
                    unit: "tokens".into(),
                    span: Some(bspan),
                });
            }
            let tokens = raw as u32;
            // Issue 1: optionally consume a unit suffix (`tokens` or `grains`).
            let unit = if let Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) = self.peek()
            {
                match id.to_ascii_lowercase().as_str() {
                    "tokens" => {
                        self.advance();
                        BudgetUnit::Tokens
                    }
                    "grains" => {
                        self.advance();
                        BudgetUnit::Grains
                    }
                    _ => BudgetUnit::Tokens,
                }
            } else {
                BudgetUnit::Tokens
            };
            // Validate budget range (CAL-E033).
            if tokens == 0 || tokens > 16000 {
                return Err(CalError::AssembleBudgetExceeded {
                    value: tokens as u64,
                    max: 16000,
                    unit: match unit {
                        BudgetUnit::Tokens => "tokens",
                        BudgetUnit::Grains => "grains",
                    }
                    .into(),
                    span: Some(bspan),
                });
            }
            Some(BudgetSpec {
                tokens,
                unit,
                span: Some(bspan),
            })
        } else {
            None
        };

        // Optional PRIORITY clause — supports two syntaxes:
        //   Weighted:  PRIORITY label1: 0.7, label2: 0.3
        //   Ordering:  PRIORITY label1 > label2 > label3
        let priority = if self.at_exact(&Token::Priority) {
            self.advance();
            // Peek after the first label to decide which syntax. Only the
            // weighted form requires a colon — anything else (including a
            // single label followed by FORMAT/WITH/end-of-stmt) is treated as
            // ordering. Without this, `PRIORITY label FORMAT ...` falls into
            // the weighted branch and fails with `expected :, found FORMAT`.
            let is_weighted = matches!(
                (self.peek_ahead(0), self.peek_ahead(1)),
                (
                    Some(_),
                    Some(SpannedToken {
                        token: Token::Colon,
                        ..
                    })
                )
            );
            if !is_weighted {
                // Ordering syntax: PRIORITY a > b > c
                let mut labels = vec![];
                loop {
                    let pspan = self.current_span();
                    let label = self.parse_label()?;
                    labels.push((label, pspan));
                    if !self.eat_exact(&Token::Gt) {
                        break;
                    }
                }
                // Assign evenly spaced weights: first=1.0, last=1/N.
                let n = labels.len() as f64;
                let specs = labels
                    .into_iter()
                    .enumerate()
                    .map(|(i, (label, pspan))| PrioritySpec {
                        label,
                        weight: (n - i as f64) / n,
                        span: Some(pspan),
                    })
                    .collect();
                Some(specs)
            } else {
                // Weighted syntax: PRIORITY label1: 0.7, label2: 0.3
                let mut specs = vec![];
                loop {
                    let pspan = self.current_span();
                    let label = self.parse_label()?;
                    self.expect_exact(&Token::Colon)?;
                    let weight = self.parse_number()?;
                    // Validate priority weight range 0.0..=1.0.
                    if !(0.0..=1.0).contains(&weight) {
                        return Err(CalError::UnexpectedToken {
                            expected: "weight between 0.0 and 1.0".into(),
                            found: format!("{}", weight),
                            span: Some(pspan),
                            suggestion: Some(
                                "PRIORITY weights must be in the range 0.0 to 1.0".into(),
                            ),
                        });
                    }
                    specs.push(PrioritySpec {
                        label,
                        weight,
                        span: Some(pspan),
                    });
                    if !self.eat_exact(&Token::Comma) {
                        break;
                    }
                }
                Some(specs)
            }
        } else {
            None
        };

        // Issue 2: validate priority labels match source labels (CAL-E035).
        if let (Some(ref prio_specs), Some(ref srcs)) = (&priority, &sources) {
            let source_labels: std::collections::HashSet<&str> =
                srcs.iter().map(|s| s.label.as_str()).collect();
            for ps in prio_specs {
                if !source_labels.contains(ps.label.as_str()) {
                    return Err(CalError::AssemblePriorityMismatch {
                        label: ps.label.clone(),
                        span: ps.span,
                    });
                }
            }
        }

        // Optional FORMAT clause (assemble-specific, before pipeline).
        let format = if self.at_exact(&Token::Format) {
            Some(self.parse_format()?)
        } else {
            None
        };

        // Optional assemble-specific WITH clause.
        // Issue 1: only consume WITH if the next token is `dedup` (an
        // ASSEMBLE-specific WITH option).  Otherwise leave WITH for the
        // top-level WITH parser to handle (e.g. `WITH rerank`).
        //
        // The list may mix dedup with general recall-tuning options
        // (`WITH dedup, recency_weight(0.7)`), so parse it with the full WITH
        // parser: `dedup` stays in `assemble_with`, and the rest go onto the
        // AssembleStmt's OWN `with_options` — scoped to this assemble's recall.
        // (They used to be pushed onto the shared `pending_with_options` field
        // and drained by the enclosing `parse_statement_full`, which bound them
        // to the WRONG query when the ASSEMBLE was nested under EXPLAIN /
        // COALESCE / parens / brace / an assemble-source.)
        let (assemble_with, assemble_with_options) = if self.at_exact(&Token::With)
            && matches!(
                self.peek_ahead(1),
                Some(SpannedToken {
                    token: Token::Dedup,
                    ..
                })
            ) {
            let mut aw = Vec::new();
            let mut opts = Vec::new();
            for opt in self.parse_with_clause()? {
                match opt {
                    WithOption::Dedup { field } => aw.push(AssembleWithOption::Dedup { field }),
                    other => opts.push(other),
                }
            }
            (aw, opts)
        } else {
            (Vec::new(), Vec::new())
        };

        // ── Out-of-order clauses are an error, not a silent detach ───────
        //
        // ASSEMBLE's clause order is fixed (OMS §8.2). Every clause is parsed
        // by an `if self.at_exact(...)` that simply doesn't fire when the
        // clause arrives late, so a misordered statement PARSED FINE and ran
        // with the clause missing: `… FORMAT markdown BUDGET 900` silently
        // dropped the budget and assembled to the 4000-token default. That is
        // a security-adjacent failure — the guard you wrote is not the guard
        // that ran — and it was a known bug (docs/facts/context-assembly.md
        // §12) rather than an unknown one. Anything still sitting here that
        // belongs to an earlier clause is now refused by name.
        if let Some(st) = self.peek() {
            let late = match st.token {
                Token::Budget => Some("BUDGET"),
                Token::Priority => Some("PRIORITY"),
                Token::For => Some("FOR"),
                Token::Where => Some("WHERE"),
                Token::Format => Some("FORMAT"),
                Token::From => Some("FROM"),
                _ => None,
            };
            if let Some(clause) = late {
                return Err(CalError::UnexpectedToken {
                    expected: "end of the ASSEMBLE statement".into(),
                    found: format!("{clause} clause out of order"),
                    span: Some(st.span),
                    suggestion: Some(
                        "ASSEMBLE clauses are ordered: ASSEMBLE \"name\" FOR \"…\" FROM … \
                         [WHERE …] BUDGET n PRIORITY … FORMAT … WITH dedup. A clause written \
                         out of order used to be dropped silently — put it in this order."
                            .into(),
                    ),
                });
            }
        }

        let span_end = self.prev_span();
        Ok(CalStatement::Assemble(AssembleStmt {
            topic,
            from,
            where_clause,
            context_name,
            sources,
            budget,
            priority,
            format,
            for_whom,
            assemble_with,
            with_options: assemble_with_options,
            streaming: false,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse the FROM clause of ASSEMBLE, supporting both single-source and
    /// multi-source (`label1: (RECALL ...), label2: (RECALL ...)`) forms.
    fn parse_assemble_from_clause(&mut self) -> CalResult<(Source, Option<Vec<NamedSource>>)> {
        // Peek ahead to check for `label:` pattern (multi-source).
        // Labels can be identifiers or keyword tokens (e.g. `recent:`).
        let is_multi_source = matches!(
            (self.peek(), self.peek_ahead(1)),
            (
                Some(st),
                Some(SpannedToken {
                    token: Token::Colon,
                    ..
                }),
            ) if matches!(st.token, Token::Ident(_)) || Self::is_word_token(&st.token)
        );

        if is_multi_source {
            let mut sources = vec![];
            loop {
                let sspan = self.current_span();
                let label = self.parse_label()?;
                self.expect_exact(&Token::Colon)?;
                // `PIN` marks the source non-degradable. It sits between the
                // colon and the body so it reads as a property of the body —
                // `guardrail: PIN LITERAL "…"`, `turns: PIN (RECALL …)` — and
                // so a source may still be LABELLED `pin`.
                let pinned = self.eat_exact(&Token::Pin);
                // `LITERAL "…"` — host text instead of a query.
                if self.at_exact(&Token::Literal) {
                    self.advance();
                    let text = self.parse_string_literal()?;
                    sources.push(NamedSource {
                        label,
                        // Placeholder: a literal source is never executed. Keeps
                        // the serialized AST shape stable for existing readers.
                        query: Box::new(CalStatement::Assemble(AssembleStmt {
                            topic: String::new(),
                            from: Source::Parameter {
                                name: "_literal".to_string(),
                            },
                            where_clause: None,
                            context_name: None,
                            sources: None,
                            budget: None,
                            priority: None,
                            format: None,
                            for_whom: None,
                            assemble_with: Vec::new(),
                            with_options: Vec::new(),
                            streaming: false,
                            span: Some(sspan),
                        })),
                        literal: Some(text),
                        pinned,
                        with_options: Vec::new(),
                        span: Some(sspan),
                    });
                    if !self.eat_exact(&Token::Comma) {
                        break;
                    }
                    continue;
                }
                // Parse the sub-query (in parentheses or bare RECALL),
                // accepting an optional WITH clause INSIDE the parens.
                let (query, inside_with) = self.parse_assemble_source_query()?;
                // Parse optional outside-paren WITH options (back-compat).
                let mut with_options = inside_with;
                let outside_with = if self.at_exact(&Token::With) && !self.peek_next_is_vars() {
                    self.parse_with_clause()?
                } else {
                    vec![]
                };
                with_options.extend(outside_with); // Q1: inside-paren first, then outside-paren
                sources.push(NamedSource {
                    label,
                    query: Box::new(query),
                    literal: None,
                    pinned,
                    with_options,
                    span: Some(sspan),
                });
                if !self.eat_exact(&Token::Comma) {
                    break;
                }
            }
            // Use the first source as the single `from` for backward compat.
            let default_from = Source::Parameter {
                name: "_multi_source".to_string(),
            };
            Ok((default_from, Some(sources)))
        } else {
            let from = self.parse_assemble_source()?;
            Ok((from, None))
        }
    }

    /// Parse ASSEMBLE-specific WITH options (dedup).
    fn parse_assemble_source(&mut self) -> CalResult<Source> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Parameter(_),
                ..
            }) => {
                let name = self.parse_parameter()?;
                Ok(Source::Parameter { name })
            }
            Some(SpannedToken {
                token: Token::LParen,
                ..
            }) => {
                // Sub-query.
                self.advance();
                self.enter_nesting()?;
                // Expect RECALL inside.
                let inner = self.parse_recall_stmt()?;
                self.leave_nesting();
                self.expect_exact(&Token::RParen)?;
                Ok(Source::Query(Box::new(inner)))
            }
            Some(SpannedToken {
                token: Token::HashLiteral(_),
                ..
            }) => {
                let mut hashes = vec![];
                while self.at(&Token::HashLiteral("".into())) {
                    hashes.push(self.parse_hash_literal()?);
                    if !self.eat_exact(&Token::Comma) {
                        break;
                    }
                }
                Ok(Source::Hashes(hashes))
            }
            Some(SpannedToken {
                token: Token::Recall,
                ..
            }) => {
                // Unparenthesised RECALL.
                let inner = self.parse_recall_stmt()?;
                Ok(Source::Query(Box::new(inner)))
            }
            // Bare grain type plural (e.g. `facts`, `events`) — synthesize an
            // implicit RECALL with optional WHERE and RECENT clauses.
            Some(SpannedToken {
                token: Token::Ident(id),
                ..
            }) if GrainTypePlural::parse(id).is_some() => {
                let span = self.current_span();
                // Extract the ident string and advance.
                let grain_type_str = if let Some(SpannedToken {
                    token: Token::Ident(id),
                    ..
                }) = self.advance()
                {
                    id.clone()
                } else {
                    "facts".to_string()
                };
                let grain_type =
                    GrainTypePlural::parse(&grain_type_str).unwrap_or(GrainTypePlural::Facts);
                let where_clause = self.parse_where_clause()?;
                let recent = self.parse_recent_clause()?;
                let inner = RecallStmt {
                    grain_type,
                    about: None,
                    where_clause,
                    recent,
                    since: None,
                    until: None,
                    like: None,
                    between: None,
                    contradictions: None,
                    limit: None,
                    as_format: None,
                    span: Some(span),
                };
                Ok(Source::Query(Box::new(inner)))
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                Err(CalError::UnexpectedToken {
                    expected:
                        "source (RECALL ..., $parameter, sha256:..., grain type, or (RECALL ...))"
                            .into(),
                    found,
                    span: Some(span),
                    suggestion: None,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "FROM source".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    // -- EXISTS -----------------------------------------------------------

    fn parse_exists(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Exists)?;

        // EXISTS can be followed by:
        // - `sha256:...` hash literal — direct existence check
        // - `$param` — parameterised hash
        // - grain type + optional WHERE — set existence check
        match self.peek() {
            Some(SpannedToken {
                token: Token::HashLiteral(_),
                ..
            }) => {
                let hash = self.parse_hash_literal()?;
                // Desugar to ExistsStmt with a WHERE hash = hash condition.
                let span_end = self.prev_span();
                Ok(CalStatement::Exists(ExistsStmt {
                    grain_type: GrainTypePlural::All,
                    where_clause: Some(WhereClause {
                        condition: Condition::Comparison {
                            field: "hash".into(),
                            comparator: Comparator::Eq,
                            value: Value::Hash { value: hash },
                            span: None,
                        },
                        span: None,
                    }),
                    about: None,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                }))
            }
            Some(SpannedToken {
                token: Token::Parameter(_),
                ..
            }) => {
                let name = self.parse_parameter()?;
                let span_end = self.prev_span();
                Ok(CalStatement::Exists(ExistsStmt {
                    grain_type: GrainTypePlural::All,
                    where_clause: Some(WhereClause {
                        condition: Condition::Comparison {
                            field: "hash".into(),
                            comparator: Comparator::Eq,
                            value: Value::Parameter { name },
                            span: None,
                        },
                        span: None,
                    }),
                    about: None,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                }))
            }
            _ => {
                // Grain-type + optional WHERE form.
                let grain_type = self.parse_grain_type_plural()?;
                let about = self.parse_about_clause()?;
                let where_clause = self.parse_where_clause()?;
                let span_end = self.prev_span();
                Ok(CalStatement::Exists(ExistsStmt {
                    grain_type,
                    where_clause,
                    about,
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                }))
            }
        }
    }

    // -- HISTORY ----------------------------------------------------------

    fn parse_history(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::History)?;

        // Optional `OF` keyword.
        self.eat_exact(&Token::Of);

        // Two forms:
        // 1. `HISTORY [OF] sha256:... [DIFF sha256:...]` — hash-based
        // 2. `HISTORY WHERE subject = ... AND relation = ...` — triple-based (Phase 2)
        if self.at_exact(&Token::Where) {
            // Phase 2: WHERE-based history lookup.
            let where_clause = self.parse_where_clause()?;

            let diff_target: Option<String> = if self.at_exact(&Token::Diff) {
                self.advance();
                Some(self.parse_hash_literal()?)
            } else {
                None
            };

            let span_end = self.prev_span();
            return Ok(CalStatement::History(HistoryStmt {
                hash: String::new(),
                where_clause,
                diff_target,
                span: Some(Span::new(
                    span_start.start,
                    span_end.end,
                    span_start.line,
                    span_start.col,
                )),
            }));
        }

        // Phase 1: hash-based history lookup.
        let hash = self.parse_hash_literal()?;

        // Optional `DIFF <hash>`.
        let diff_target: Option<String> = if self.at_exact(&Token::Diff) {
            self.advance();
            Some(self.parse_hash_literal()?)
        } else {
            None
        };

        let span_end = self.prev_span();
        Ok(CalStatement::History(HistoryStmt {
            hash,
            where_clause: None,
            diff_target,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- EXPLAIN ----------------------------------------------------------

    fn parse_explain(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Explain)?;
        let inner = self.parse_statement()?;
        let span_end = self.prev_span();

        // EXPLAIN only wraps statements that produce an execution plan
        // (RECALL, ASSEMBLE, SetOp, ADD, SUPERSEDE, REVERT, BATCH, COALESCE).
        // DESCRIBE / template / saved-query statements have no plan.
        match &inner {
            CalStatement::Describe(_)
            | CalStatement::DefineTemplate(_)
            | CalStatement::DropTemplate(_)
            | CalStatement::DefineQuery(_)
            | CalStatement::DropQuery(_) => {
                return Err(CalError::UnexpectedToken {
                    expected: "RECALL, ASSEMBLE, set operation, ADD, SUPERSEDE, REVERT, BATCH, or COALESCE".into(),
                    found: format!("{} statement", match &inner {
                        CalStatement::Describe(_) => "DESCRIBE",
                        CalStatement::DefineTemplate(_) => "DEFINE TEMPLATE",
                        CalStatement::DropTemplate(_) => "DROP TEMPLATE",
                        CalStatement::DefineQuery(_) => "DEFINE QUERY",
                        CalStatement::DropQuery(_) => "DROP QUERY",
                        _ => unreachable!(),
                    }),
                    span: Some(span_start),
                    suggestion: Some(
                        "EXPLAIN may only wrap statements that produce an execution plan (spec §8.5).".into(),
                    ),
                });
            }
            _ => {}
        }

        Ok(CalStatement::Explain(ExplainStmt {
            inner: Box::new(inner),
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- DESCRIBE ---------------------------------------------------------

    fn parse_describe(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Describe)?;

        let target = match self.peek() {
            Some(SpannedToken {
                token: Token::Ident(s),
                ..
            }) => {
                let s = s.clone();
                // Try to parse as grain type first.
                if let Some(gt) = GrainTypePlural::parse(&s) {
                    self.advance();
                    DescribeTarget::GrainType(gt)
                } else if s.eq_ignore_ascii_case("schema") {
                    self.advance();
                    DescribeTarget::Schema
                } else if s.eq_ignore_ascii_case("capabilities") {
                    self.advance();
                    DescribeTarget::Capabilities
                } else if s.eq_ignore_ascii_case("server") {
                    self.advance();
                    DescribeTarget::Server
                } else if s.eq_ignore_ascii_case("fields") {
                    self.advance();
                    // Optional grain type after FIELDS.
                    let gt = self.parse_grain_type_plural_opt()?;
                    DescribeTarget::Fields(gt)
                } else if s.eq_ignore_ascii_case("templates") {
                    self.advance();
                    DescribeTarget::Templates
                } else if s.eq_ignore_ascii_case("grammar") {
                    self.advance();
                    DescribeTarget::Grammar
                } else if s.eq_ignore_ascii_case("queries") {
                    self.advance();
                    DescribeTarget::Queries
                } else if s.eq_ignore_ascii_case("principal") {
                    self.advance();
                    let name = self.parse_string_literal()?;
                    DescribeTarget::Principal(name)
                } else if s.eq_ignore_ascii_case("loop") {
                    self.advance();
                    DescribeTarget::Loop
                } else if s.eq_ignore_ascii_case("analyzers") {
                    self.advance();
                    DescribeTarget::Analyzers
                } else if s.eq_ignore_ascii_case("outcomes") {
                    self.advance();
                    DescribeTarget::Outcomes
                } else if s.eq_ignore_ascii_case("policy") {
                    self.advance();
                    DescribeTarget::LoopPolicy
                } else if s.eq_ignore_ascii_case("stats") {
                    self.advance();
                    DescribeTarget::Stats
                } else if s.eq_ignore_ascii_case("integrity") {
                    self.advance();
                    DescribeTarget::Integrity
                } else {
                    self.advance();
                    DescribeTarget::Schema
                }
            }
            // QUERY is a keyword token, not an Ident — handle DESCRIBE QUERY "name".
            Some(SpannedToken {
                token: Token::Query,
                ..
            }) => {
                self.advance(); // consume QUERY
                let name = self.parse_string_literal()?;
                DescribeTarget::Query(name)
            }
            _ => DescribeTarget::Schema,
        };

        let span_end = self.prev_span();
        Ok(CalStatement::Describe(DescribeStmt {
            target,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- BATCH ------------------------------------------------------------

    fn parse_batch(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Batch)?;
        self.expect_exact(&Token::LBrace)?;
        self.enter_nesting()?;

        let mut entries = vec![];
        while !self.at_exact(&Token::RBrace) && !self.at_end() {
            if entries.len() >= MAX_BATCH_ENTRIES {
                return Err(CalError::TooManyPipelineStages {
                    count: entries.len() + 1,
                    max: MAX_BATCH_ENTRIES,
                    span: Some(self.current_span()),
                });
            }
            // Optional label: `label:` — any token followed by `:` that is
            // NOT itself a statement-starter is treated as a label.  The
            // label is discarded; only the statement that follows matters.
            // This handles both `ident:` and keyword-reused-as-label cases
            // like `recent:` or `exists:`.
            let is_label = match self.peek() {
                Some(st) if !st.token.is_statement_starter() => {
                    self.peek_ahead(1).map(|t| &t.token) == Some(&Token::Colon)
                }
                _ => false,
            };
            if is_label {
                self.advance(); // label token
                self.advance(); // colon
            }
            // Parse the full statement including pipeline, FORMAT, WITH options.
            let (stmt, pipeline, with_options, format, user_vars) = self.parse_statement_full()?;
            entries.push(BatchEntry {
                statement: stmt,
                pipeline,
                with_options,
                format,
                user_vars,
            });
            // Statements separated by comma or semicolon.
            self.eat_exact(&Token::Comma);
            self.eat_exact(&Token::Semicolon);
        }

        self.leave_nesting();
        self.expect_exact(&Token::RBrace)?;

        // BATCH requires ≥1 entry per spec §8.7.
        if entries.is_empty() {
            return Err(CalError::UnexpectedToken {
                expected: "at least one statement inside BATCH { ... }".into(),
                found: "empty batch".into(),
                span: Some(span_start),
                suggestion: Some("BATCH requires one or more statements (spec §8.7).".into()),
            });
        }

        let span_end = self.prev_span();
        Ok(CalStatement::Batch(BatchStmt {
            statements: entries,
            labeled: None,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- COALESCE ---------------------------------------------------------

    fn parse_coalesce(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Coalesce)?;

        // Two forms:
        // Phase 1: `COALESCE(stmt1, stmt2, ...)`
        // Phase 2: `COALESCE { stmt } OR { stmt } [ELSE { stmt }]`
        if self.at_exact(&Token::LBrace) {
            // Phase 2: brace-delimited multi-branch form.
            return self.parse_coalesce_braces(span_start);
        }

        // Phase 1: parenthesised form.
        self.expect_exact(&Token::LParen)?;
        self.enter_nesting()?;

        let mut statements = vec![];
        while !self.at_exact(&Token::RParen) && !self.at_end() {
            let stmt = self.parse_statement()?;
            statements.push(stmt);
            if !self.eat_exact(&Token::Comma) {
                break;
            }
        }

        self.leave_nesting();
        self.expect_exact(&Token::RParen)?;

        // COALESCE(...) requires 2-5 branches per spec §8.13.
        const COALESCE_MIN: usize = 2;
        const COALESCE_MAX: usize = 5;
        if statements.len() < COALESCE_MIN {
            return Err(CalError::UnexpectedToken {
                expected: format!("at least {} branches in COALESCE(...)", COALESCE_MIN),
                found: format!("{} branches", statements.len()),
                span: Some(span_start),
                suggestion: Some("COALESCE requires 2-5 RECALL branches (spec §8.13).".into()),
            });
        }
        if statements.len() > COALESCE_MAX {
            return Err(CalError::CoalesceTooManyBranches {
                count: statements.len(),
                max: COALESCE_MAX,
                span: Some(span_start),
            });
        }

        // Build CoalesceStmt with branches from the parsed inner statements.
        let grain_type = GrainTypePlural::All;
        let span_end = self.prev_span();

        Ok(CalStatement::Coalesce(CoalesceStmt {
            grain_type,
            where_clause: None,
            branches: statements
                .into_iter()
                .map(|q| CoalesceBranch {
                    query: q,
                    span: None,
                })
                .collect(),
            else_branch: None,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse Phase 2 brace-delimited COALESCE:
    /// `COALESCE { stmt } OR { stmt } [ELSE { stmt }]`
    fn parse_coalesce_braces(&mut self, span_start: Span) -> CalResult<CalStatement> {
        let mut branches = vec![];

        // Parse first branch: `{ stmt }`.
        let branch = self.parse_brace_statement()?;
        branches.push(CoalesceBranch {
            span: Some(self.prev_span()),
            query: branch,
        });

        // Parse additional `OR { stmt }` branches.
        while self.at_exact(&Token::Or) {
            self.advance(); // consume OR
            let branch = self.parse_brace_statement()?;
            branches.push(CoalesceBranch {
                span: Some(self.prev_span()),
                query: branch,
            });
        }

        // Optional `ELSE { stmt }`.
        let else_branch = if self.at_exact(&Token::Ident("ELSE".into()))
            || self
                .peek()
                .map(|st| st.token.description().eq_ignore_ascii_case("ELSE"))
                .unwrap_or(false)
        {
            // Check if current token is "ELSE" identifier.
            if let Some(SpannedToken {
                token: Token::Ident(s),
                ..
            }) = self.peek()
            {
                if s.eq_ignore_ascii_case("else") {
                    self.advance();
                    let stmt = self.parse_brace_statement()?;
                    Some(Box::new(stmt))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let span_end = self.prev_span();
        Ok(CalStatement::Coalesce(CoalesceStmt {
            grain_type: GrainTypePlural::All,
            where_clause: None,
            branches,
            else_branch,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse a statement enclosed in braces: `{ <statement> }`.
    fn parse_brace_statement(&mut self) -> CalResult<CalStatement> {
        self.expect_exact(&Token::LBrace)?;
        self.enter_nesting()?;
        let stmt = self.parse_statement()?;
        self.leave_nesting();
        self.expect_exact(&Token::RBrace)?;
        Ok(stmt)
    }

    // -- ADD (Tier 1) -----------------------------------------------------

    fn parse_add(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Add)?;

        // Check for `ADD workflow "name" ...` (graph syntax).
        if let Some(SpannedToken {
            token: Token::Ident(s),
            ..
        }) = self.peek()
        {
            if s.eq_ignore_ascii_case("workflow") {
                self.advance(); // consume "workflow"
                return self.parse_add_workflow(span_start);
            }
        }

        let grain_type = self.parse_grain_type_singular()?;

        // `SET field = value` clauses.
        let mut fields = vec![];
        let mut seen_fields = std::collections::HashSet::new();
        while self.at_exact(&Token::Set) {
            self.advance();
            let fspan = self.current_span();
            let field = self.parse_identifier()?;
            self.expect_exact(&Token::Eq)?;
            let value = self.parse_value()?;
            if !seen_fields.insert(field.clone()) {
                self.warnings.push(CalWarning::DuplicateSetField {
                    field: field.clone(),
                    span: Some(fspan),
                });
            }
            fields.push(FieldAssignment {
                field,
                value,
                span: Some(fspan),
            });
        }

        if fields.is_empty() {
            return Err(CalError::MissingSetClause {
                span: Some(self.current_span()),
            });
        }

        // Optional WITH clause for ADD options (must come before REASON).
        let with_options = if self.at_exact(&Token::With) {
            self.parse_add_with_clause()?
        } else {
            vec![]
        };

        // Mandatory REASON / BECAUSE clause (required for all Tier 1 writes).
        let reason = self.parse_reason_clause()?;

        let span_end = self.prev_span();
        Ok(CalStatement::Add(AddStmt {
            grain_type,
            fields,
            reason,
            with_options,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- ADD WORKFLOW (graph syntax) ----------------------------------------

    /// Parse `ADD workflow "name" [ON "trigger"] graph... [BIND ...] REASON "..."`
    fn parse_add_workflow(&mut self, span_start: Span) -> CalResult<CalStatement> {
        // Positional name: `ADD workflow "name"`
        let name = self.parse_string_literal()?;

        // `ON "trigger"` was removed in 1.3. It set a free-text field that
        // nothing ever read — neither the scheduler nor the driver — so it
        // described an activation condition that could not activate anything.
        // Refuse it by name rather than silently ignoring it, so anyone with the
        // old syntax is told where triggers actually live now.
        if self.at_exact(&Token::On) {
            return Err(CalError::UnexpectedToken {
                expected: "the plan's graph — `ON \"...\"` was removed in 1.3; a trigger is now \
                           its own grain that points at this plan (see `areev trigger add`)"
                    .into(),
                found: "ON".into(),
                span: Some(self.current_span()),
                suggestion: Some(
                    "declare a trigger with `areev trigger add --workflow <this plan's hash>`"
                        .into(),
                ),
            });
        }

        // Parse graph lines (edges).
        let (nodes, edges) = self.parse_workflow_graph()?;

        // Parse BIND clauses.
        let mut bindings = vec![];
        while self.at_exact(&Token::Bind) {
            bindings.push(self.parse_bind_clause()?);
        }

        // Optional WITH clause.
        let with_options = if self.at_exact(&Token::With) {
            self.parse_add_with_clause()?
        } else {
            vec![]
        };

        // REASON / BECAUSE is required.
        let reason = self.parse_reason_clause()?;

        let span_end = self.prev_span();
        Ok(CalStatement::AddWorkflow(AddWorkflowStmt {
            name,
            nodes,
            edges,
            bindings,
            reason,
            with_options,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse the graph body: arrow chains building nodes + edges.
    ///
    /// Grammar:
    /// ```text
    /// graph       = chain { chain }
    /// chain       = node_or_group { "->" node_or_group [WHEN cond] [* n] }
    /// node_or_group = node | "(" node { "," node } ")"
    /// node        = identifier | string_literal
    /// ```
    fn parse_workflow_graph(&mut self) -> CalResult<(Vec<String>, Vec<GraphEdge>)> {
        let mut nodes: Vec<String> = Vec::new();
        let mut edges: Vec<GraphEdge> = Vec::new();

        // Helper: ensure a node is recorded (deduplicated, preserves order).
        let ensure_node = |name: &str, nodes: &mut Vec<String>| {
            if !nodes.contains(&name.to_string()) {
                nodes.push(name.to_string());
            }
        };

        // Parse at least one chain.
        let mut parsed_any = false;
        while self.is_graph_line_start() {
            parsed_any = true;

            // Parse the first node or group in this chain.
            let mut sources = self.parse_node_or_group()?;
            for s in &sources {
                ensure_node(s, &mut nodes);
            }

            // Parse chain: `-> target [WHEN "..."] [* N]`
            while self.eat_exact(&Token::Arrow) {
                // Detect double arrow: `a -> -> b`
                if self.at_exact(&Token::Arrow) {
                    return Err(CalError::UnexpectedToken {
                        expected: "node name after '->'".into(),
                        found: "another '->'".into(),
                        span: Some(self.current_span()),
                        suggestion: Some(
                            "remove the extra '->' — chain nodes with single arrows: a -> b -> c"
                                .into(),
                        ),
                    });
                }
                let targets = self.parse_node_or_group()?;
                for t in &targets {
                    ensure_node(t, &mut nodes);
                }

                // Optional WHEN condition.
                let cond = if self.eat_exact(&Token::When) {
                    Some(self.parse_string_literal()?)
                } else {
                    None
                };

                // Optional * N repeat.
                let repeat = if self.eat_exact(&Token::Asterisk) {
                    // Detect non-number after `*`: `a -> b * abc`
                    if !matches!(
                        self.peek(),
                        Some(SpannedToken {
                            token: Token::NumberLiteral(_),
                            ..
                        })
                    ) {
                        let found = self
                            .peek()
                            .map(|st| st.token.description())
                            .unwrap_or_else(|| "<end of query>".into());
                        return Err(CalError::UnexpectedToken {
                            expected: "number after '*' for retry count".into(),
                            found,
                            span: Some(self.current_span()),
                            suggestion: Some("* N sets the retry count — e.g.: a -> b * 3".into()),
                        });
                    }
                    let n = self.parse_number()? as u32;
                    if n == 0 {
                        return Err(CalError::UnexpectedToken {
                            expected: "repeat count >= 1".into(),
                            found: "0".into(),
                            span: Some(self.prev_span()),
                            suggestion: Some(
                                "repeat count must be at least 1 — e.g.: a -> b * 1".into(),
                            ),
                        });
                    }
                    Some(n)
                } else {
                    None
                };

                // Create edges: every source -> every target.
                for s in &sources {
                    for t in &targets {
                        edges.push(GraphEdge {
                            src: s.clone(),
                            dst: t.clone(),
                            cond: cond.clone(),
                            repeat,
                        });
                    }
                }

                // Targets become sources for the next `->` segment.
                // This handles `a -> b -> c` as a->b, b->c.
                sources = targets;
            }
        }

        // Catch misplaced WHEN — it must follow a `->` edge, not a bare node.
        if self.at_exact(&Token::When) {
            return Err(CalError::UnexpectedToken {
                expected: "'->' edge before WHEN condition".into(),
                found: "WHEN".into(),
                span: Some(self.current_span()),
                suggestion: Some(
                    "WHEN must follow a -> edge, not a node — e.g.: a -> b WHEN \"condition\""
                        .into(),
                ),
            });
        }
        // Catch a stray arrow after the graph loop exited (shouldn't happen
        // normally since the inner while eats arrows, but guards edge cases).
        if self.at_exact(&Token::Arrow) && parsed_any {
            return Err(CalError::UnexpectedToken {
                expected: "node name after '->'".into(),
                found: self
                    .peek()
                    .map(|st| st.token.description())
                    .unwrap_or_else(|| "<end of query>".into()),
                span: Some(self.current_span()),
                suggestion: Some("expected a node name or parallel group after '->'".into()),
            });
        }

        if !parsed_any {
            return Err(CalError::UnexpectedToken {
                expected: "at least one node in workflow graph".into(),
                found: self
                    .peek()
                    .map(|st| st.token.description())
                    .unwrap_or_else(|| "<end of query>".into()),
                span: Some(self.current_span()),
                suggestion: Some(
                    "workflow body must contain node names and arrows, e.g.: build -> test -> deploy"
                        .into(),
                ),
            });
        }

        Ok((nodes, edges))
    }

    /// Check if the current token could start a graph line
    /// (an identifier, string literal, or opening paren).
    fn is_graph_line_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(SpannedToken {
                token: Token::Ident(_)
                    | Token::StringLiteral(_)
                    | Token::LParen
                    | Token::Run
                    | Token::Query,
                ..
            })
        )
    }

    /// Parse a node or parallel group: `node` or `(node, node, ...)`.
    fn parse_node_or_group(&mut self) -> CalResult<Vec<String>> {
        if self.eat_exact(&Token::LParen) {
            self.enter_nesting()?;
            let mut group = vec![self.parse_node_name()?];
            while self.eat_exact(&Token::Comma) {
                group.push(self.parse_node_name()?);
            }
            self.leave_nesting();
            if !self.eat_exact(&Token::RParen) {
                let found = self
                    .peek()
                    .map(|st| st.token.description())
                    .unwrap_or_else(|| "<end of query>".into());
                return Err(CalError::UnexpectedToken {
                    expected: "')' to close parallel group".into(),
                    found,
                    span: Some(self.current_span()),
                    suggestion: Some(
                        "close the parallel group with ')' — e.g.: (build, test) -> deploy".into(),
                    ),
                });
            }
            Ok(group)
        } else {
            Ok(vec![self.parse_node_name()?])
        }
    }

    /// Parse a single node name: bare identifier or quoted string.
    fn parse_node_name(&mut self) -> CalResult<String> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Ident(s),
                ..
            }) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            // Allow keywords that are also valid as workflow node names.
            Some(SpannedToken {
                token: Token::Run, ..
            }) => {
                self.advance();
                Ok("run".to_string())
            }
            Some(SpannedToken {
                token: Token::Query,
                ..
            }) => {
                self.advance();
                Ok("query".to_string())
            }
            // CAL 1.3 statement keywords that are everyday node names —
            // review/approval workflows use exactly these words.
            Some(SpannedToken {
                token:
                    t @ (Token::Approve
                    | Token::Reject
                    | Token::Apply
                    | Token::Rollback
                    | Token::Grant
                    | Token::Revoke
                    | Token::Show
                    | Token::Merge
                    | Token::Related
                    | Token::Novelty
                    | Token::Remember
                    | Token::Entity),
                ..
            }) => {
                let name = t.description().to_ascii_lowercase();
                self.advance();
                Ok(name)
            }
            Some(SpannedToken {
                token: Token::StringLiteral(_),
                ..
            }) => self.parse_string_literal(),
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                // Provide context-specific suggestions for common mistakes.
                let suggestion = match &st.token {
                    Token::Reason | Token::Because => Some(
                        "expected node name after '->' — the arrow is dangling. \
                         Add a target node: a -> b REASON \"...\""
                            .into(),
                    ),
                    Token::RParen => Some(
                        "parallel group must contain at least one node — \
                         e.g.: (build, test)"
                            .into(),
                    ),
                    _ => Some(
                        "reserved words (ON, WHEN, BIND, REASON) must be quoted to use as node names"
                            .into(),
                    ),
                };
                Err(CalError::UnexpectedToken {
                    expected: "node name (identifier or quoted string)".into(),
                    found,
                    span: Some(span),
                    suggestion,
                })
            }
            None => Err(CalError::UnexpectedToken {
                expected: "node name after '->'".into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: Some("the arrow is dangling — add a target node: a -> b".into()),
            }),
        }
    }

    /// Parse `BIND node = sha256:hash`.
    fn parse_bind_clause(&mut self) -> CalResult<BindClause> {
        self.expect_exact(&Token::Bind)?;
        let node = self.parse_node_name()?;
        self.expect_exact(&Token::Eq)?;
        let hash = self.parse_hash_literal()?;
        Ok(BindClause { node, hash })
    }

    /// Parse `REASON "..."` or `BECAUSE "..."`.
    fn parse_reason_clause(&mut self) -> CalResult<String> {
        if !self.at_exact(&Token::Reason) && !self.at_exact(&Token::Because) {
            return Err(CalError::MissingReason {
                span: Some(self.current_span()),
            });
        }
        self.advance();
        let rspan = self.current_span();
        let reason = self.parse_string_literal()?;
        if reason.len() > MAX_REASON_LENGTH {
            return Err(CalError::ReasonTooLong {
                length: reason.len(),
                max: MAX_REASON_LENGTH,
                span: Some(rspan),
            });
        }
        Ok(reason)
    }

    // -- ADD WITH clause --------------------------------------------------

    fn parse_add_with_clause(&mut self) -> CalResult<Vec<AddWithOption>> {
        self.expect_exact(&Token::With)?;
        let mut options = vec![];
        loop {
            if let Some(opt) = self.parse_add_with_option()? {
                options.push(opt);
            }
            if !self.eat_exact(&Token::Comma) {
                break;
            }
        }
        Ok(options)
    }

    fn parse_add_with_option(&mut self) -> CalResult<Option<AddWithOption>> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::ExtractEventDate,
                ..
            }) => {
                self.advance();
                Ok(Some(AddWithOption::ExtractEventDate))
            }
            Some(SpannedToken {
                token: Token::AutoRelate,
                ..
            }) => {
                self.advance();
                Ok(Some(AddWithOption::AutoRelate))
            }
            Some(SpannedToken {
                token: Token::ExtractMemories,
                ..
            }) => {
                self.advance();
                Ok(Some(AddWithOption::ExtractMemories))
            }
            Some(SpannedToken {
                token: Token::SyncOption,
                ..
            }) => {
                self.advance();
                Ok(Some(AddWithOption::Sync))
            }
            Some(SpannedToken { token: Token::Ident(w), .. })
                if w.eq_ignore_ascii_case("occurrence") =>
            {
                self.advance();
                Ok(Some(AddWithOption::Occurrence))
            }
            Some(st) => {
                let found = st.token.description();
                let span = st.span;
                self.warnings.push(CalWarning::UnknownExtensionOption {
                    option: found,
                    span: Some(span),
                });
                self.advance();
                Ok(None)
            }
            None => Err(CalError::UnexpectedToken {
                expected:
                    "ADD WITH option (extract_event_date, auto_relate, extract_memories, sync)"
                        .into(),
                found: "<end of query>".into(),
                span: None,
                suggestion: None,
            }),
        }
    }

    // -- ACCUMULATE (Tier 1) ----------------------------------------------

    fn parse_accumulate(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Accumulate)?;

        // Grain type (required).
        let grain_type = self.parse_grain_type_singular()?;

        // Resolution mode: hash literal OR WHERE clause.
        let target = if let Some(SpannedToken {
            token: Token::HashLiteral(_),
            ..
        }) = self.peek()
        {
            let hash = self.parse_hash_literal()?;
            AccumulateTarget::Hash { hash }
        } else if self.at_exact(&Token::Where) {
            self.advance();
            // Parse WHERE subject = "..." relation = "..." [namespace = "..."]
            let mut subject: Option<String> = None;
            let mut relation: Option<String> = None;
            let mut namespace: Option<String> = None;
            loop {
                // Skip optional AND connectors between field assignments.
                self.eat_exact(&Token::And);
                match self.peek() {
                    Some(SpannedToken {
                        token: Token::Ident(id),
                        ..
                    }) if id.eq_ignore_ascii_case("subject") => {
                        self.advance();
                        self.expect_exact(&Token::Eq)?;
                        subject = Some(self.parse_string_literal()?);
                    }
                    Some(SpannedToken {
                        token: Token::Ident(id),
                        ..
                    }) if id.eq_ignore_ascii_case("relation") => {
                        self.advance();
                        self.expect_exact(&Token::Eq)?;
                        relation = Some(self.parse_string_literal()?);
                    }
                    Some(SpannedToken {
                        token: Token::Ident(id),
                        ..
                    }) if id.eq_ignore_ascii_case("namespace") => {
                        self.advance();
                        self.expect_exact(&Token::Eq)?;
                        namespace = Some(self.parse_string_literal()?);
                    }
                    _ => break,
                }
            }
            let subject = subject.ok_or_else(|| CalError::UnexpectedToken {
                expected: "subject = \"...\" in WHERE clause".into(),
                found: self
                    .peek()
                    .map(|st| format!("{:?}", st.token))
                    .unwrap_or_else(|| "<end of query>".into()),
                span: Some(self.current_span()),
                suggestion: Some("ACCUMULATE WHERE requires subject = \"...\"".into()),
            })?;
            let relation = relation.ok_or_else(|| CalError::UnexpectedToken {
                expected: "relation = \"...\" in WHERE clause".into(),
                found: self
                    .peek()
                    .map(|st| format!("{:?}", st.token))
                    .unwrap_or_else(|| "<end of query>".into()),
                span: Some(self.current_span()),
                suggestion: Some("ACCUMULATE WHERE requires relation = \"...\"".into()),
            })?;
            AccumulateTarget::TipResolved {
                subject,
                relation,
                namespace,
            }
        } else {
            return Err(CalError::UnexpectedToken {
                expected: "hash literal or WHERE clause".into(),
                found: self
                    .peek()
                    .map(|st| format!("{:?}", st.token))
                    .unwrap_or_else(|| "<end of query>".into()),
                span: Some(self.current_span()),
                suggestion: Some(
                    "ACCUMULATE requires either a hash or WHERE subject = ... relation = ..."
                        .into(),
                ),
            });
        };

        // Parse ADD and SET operations.
        let mut add_ops = Vec::new();
        let mut set_ops = Vec::new();
        loop {
            if self.at_exact(&Token::Add) {
                self.advance();
                let fspan = self.current_span();
                let field = self.parse_identifier()?;
                self.expect_exact(&Token::Eq)?;
                let value = self.parse_number()?;
                add_ops.push(DeltaOp {
                    field,
                    delta: value,
                    span: Some(fspan),
                });
            } else if self.at_exact(&Token::Set) {
                self.advance();
                let fspan = self.current_span();
                let field = self.parse_identifier()?;
                self.expect_exact(&Token::Eq)?;
                let value = self.parse_value()?;
                set_ops.push(FieldAssignment {
                    field,
                    value,
                    span: Some(fspan),
                });
            } else {
                break;
            }
        }

        if add_ops.is_empty() {
            return Err(CalError::MissingAccumulateOps {
                span: Some(self.current_span()),
            });
        }

        // REASON / BECAUSE is required.
        if !self.at_exact(&Token::Reason) && !self.at_exact(&Token::Because) {
            return Err(CalError::MissingReason {
                span: Some(self.current_span()),
            });
        }
        self.advance();
        let rspan = self.current_span();
        let reason = self.parse_string_literal()?;
        if reason.len() > MAX_REASON_LENGTH {
            return Err(CalError::ReasonTooLong {
                length: reason.len(),
                max: MAX_REASON_LENGTH,
                span: Some(rspan),
            });
        }

        let span_end = self.prev_span();
        Ok(CalStatement::Accumulate(AccumulateStmt {
            grain_type,
            target,
            add_ops,
            set_ops,
            reason,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- SUPERSEDE (Tier 1) -----------------------------------------------

    fn parse_supersede(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Supersede)?;

        let hash = self.parse_hash_literal()?;

        // Detect workflow graph supersede: after the hash, a graph line start
        // (identifier, string, paren) instead of SET means a workflow
        // supersede. `ON` no longer participates — see the note in
        // `parse_add_workflow`.
        if self.is_graph_line_start() {
            return self.parse_supersede_workflow(span_start, hash);
        }

        // One or more `SET field = value` clauses.
        let mut set_clauses = vec![];
        while self.at_exact(&Token::Set) {
            self.advance();
            let fspan = self.current_span();
            let field = self.parse_identifier()?;
            self.expect_exact(&Token::Eq)?;
            let value = self.parse_value()?;
            set_clauses.push(FieldAssignment {
                field,
                value,
                span: Some(fspan),
            });
        }

        if set_clauses.is_empty() {
            return Err(CalError::MissingSetClause {
                span: Some(self.current_span()),
            });
        }

        // REASON / BECAUSE is required for write statements.
        if !self.at_exact(&Token::Reason) && !self.at_exact(&Token::Because) {
            return Err(CalError::MissingReason {
                span: Some(self.current_span()),
            });
        }
        self.advance();
        let rspan = self.current_span();
        let reason = self.parse_string_literal()?;
        if reason.len() > MAX_REASON_LENGTH {
            return Err(CalError::ReasonTooLong {
                length: reason.len(),
                max: MAX_REASON_LENGTH,
                span: Some(rspan),
            });
        }

        let span_end = self.prev_span();
        Ok(CalStatement::Supersede(SupersedeStmt {
            hash,
            set_clauses,
            reason,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `SUPERSEDE <hash> [ON "trigger"] graph... [BIND ...] REASON "..."`
    fn parse_supersede_workflow(
        &mut self,
        span_start: Span,
        hash: String,
    ) -> CalResult<CalStatement> {
        let (nodes, edges) = self.parse_workflow_graph()?;

        let mut bindings = vec![];
        while self.at_exact(&Token::Bind) {
            bindings.push(self.parse_bind_clause()?);
        }

        let reason = self.parse_reason_clause()?;

        let span_end = self.prev_span();
        Ok(CalStatement::SupersedeWorkflow(SupersedeWorkflowStmt {
            hash,
            nodes,
            edges,
            bindings,
            reason,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- REVERT (Tier 1) -------------------------------------------------

    /// `FORGET <hash>` — tombstone a single grain by content address.
    ///
    /// Only the hash form is reachable from CAL text. `FORGET USER`/`SCOPE`
    /// exist in the AST but have no store backing, and PURGE stays outside the
    /// text grammar. Execution is gated by `allow_destructive_ops`; the parser
    /// always accepts it and the executor returns `Unsupported` when disabled.
    fn parse_forget(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Forget)?;

        // `FORGET SUBJECT "<id>"` — identity-scoped erasure (CAL 1.3 §8.14).
        // USER/SCOPE remain out of the text grammar: SUBJECT is the one
        // spelled form, matching the host verbs and the spec.
        if let Some(SpannedToken { token: Token::Ident(word), span, .. }) = self.peek() {
            let word_up = word.to_ascii_uppercase();
            let span = *span;
            match word_up.as_str() {
                "SUBJECT" => {
                    self.advance();
                    let user_id = self.parse_string_literal()?;
                    // Optional `WITH text_mentions`.
                    let mut text_mentions = false;
                    if self.at_exact(&Token::With) {
                        self.advance();
                        match self.peek() {
                            Some(SpannedToken { token: Token::Ident(opt), .. })
                                if opt.eq_ignore_ascii_case("text_mentions") =>
                            {
                                self.advance();
                                text_mentions = true;
                            }
                            other => {
                                return Err(CalError::UnexpectedToken {
                                    expected: "text_mentions".into(),
                                    found: other
                                        .map(|t| t.token.description())
                                        .unwrap_or_else(|| "end of input".into()),
                                    span: other.map(|t| t.span),
                                    suggestion: Some(
                                        "FORGET SUBJECT supports exactly one option: \
                                         WITH text_mentions"
                                            .into(),
                                    ),
                                });
                            }
                        }
                    }
                    // BECAUSE is mandatory on the subject form — an identity
                    // erasure without a recorded reason is not auditable.
                    if !self.at_exact(&Token::Reason) && !self.at_exact(&Token::Because) {
                        return Err(CalError::MissingReason {
                            span: Some(self.current_span()),
                        });
                    }
                    self.advance();
                    let reason = self.parse_string_literal()?;
                    let span_end = self.prev_span();
                    return Ok(CalStatement::Forget(ForgetStmt {
                        target: ForgetTarget::User { user_id },
                        reason: Some(reason),
                        text_mentions,
                        span: Some(Span::new(
                            span_start.start,
                            span_end.end,
                            span_start.line,
                            span_start.col,
                        )),
                    }));
                }
                "USER" | "SCOPE" => {
                    return Err(CalError::UnexpectedToken {
                        expected: "a grain hash or SUBJECT".into(),
                        found: word_up,
                        span: Some(span),
                        suggestion: Some(
                            "identity erasure is spelled FORGET SUBJECT \"<id>\" \
                             BECAUSE \"<why>\"; scope erasure is not part of the \
                             text grammar"
                                .into(),
                        ),
                    });
                }
                _ => {}
            }
        }

        let hash = self.parse_hash_literal()?;
        // BECAUSE is optional on the hash form (it predates the requirement)
        // but recorded when given.
        let reason = if self.at_exact(&Token::Reason) || self.at_exact(&Token::Because) {
            self.advance();
            Some(self.parse_string_literal()?)
        } else {
            None
        };
        let span_end = self.prev_span();
        Ok(CalStatement::Forget(ForgetStmt {
            target: ForgetTarget::Hash { hash },
            reason,
            text_mentions: false,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `REPORT SUBJECT "<id>" [WITH text_mentions]` — the read-only
    /// DSAR selection (OMS 1.6 draft). No BECAUSE: it is a pure read, and
    /// reads don't carry reasons.
    fn parse_report_subject(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Report)?;
        match self.peek() {
            Some(SpannedToken { token: Token::Ident(word), .. })
                if word.eq_ignore_ascii_case("SUBJECT") =>
            {
                self.advance();
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "SUBJECT".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: Some(
                        "the DSAR read is spelled REPORT SUBJECT \"<id>\" \
                         [WITH text_mentions]"
                            .into(),
                    ),
                });
            }
        }
        let subject_id = self.parse_string_literal()?;
        let mut text_mentions = false;
        if self.at_exact(&Token::With) {
            self.advance();
            match self.peek() {
                Some(SpannedToken { token: Token::Ident(opt), .. })
                    if opt.eq_ignore_ascii_case("text_mentions") =>
                {
                    self.advance();
                    text_mentions = true;
                }
                other => {
                    return Err(CalError::UnexpectedToken {
                        expected: "text_mentions".into(),
                        found: other
                            .map(|t| t.token.description())
                            .unwrap_or_else(|| "end of input".into()),
                        span: other.map(|t| t.span),
                        suggestion: Some(
                            "REPORT SUBJECT supports exactly one option: \
                             WITH text_mentions"
                                .into(),
                        ),
                    });
                }
            }
        }
        let span_end = self.prev_span();
        Ok(CalStatement::ReportSubject(ReportSubjectStmt {
            subject_id,
            text_mentions,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `PURGE OLDER THAN <n><d|h|m> [TYPE <grain-type>]
    /// [IN "<namespace>"] [LIMIT <n>] BECAUSE "<why>"` (CAL 1.3 §8.14).
    fn parse_purge(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Purge)?;

        let expect_word = |me: &mut Self, word: &str| -> CalResult<()> {
            match me.peek() {
                Some(SpannedToken { token: Token::Ident(w), .. })
                    if w.eq_ignore_ascii_case(word) =>
                {
                    me.advance();
                    Ok(())
                }
                other => Err(CalError::UnexpectedToken {
                    expected: word.into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: Some(
                        "the retention sweep is spelled PURGE OLDER THAN <n><d|h|m> \
                         [TYPE <t>] [IN \"<ns>\"] BECAUSE \"<why>\""
                            .into(),
                    ),
                }),
            }
        };
        expect_word(self, "OLDER")?;
        expect_word(self, "THAN")?;

        // `90d` lexes as NumberLiteral(90) + Ident("d").
        let n = match self.peek() {
            Some(SpannedToken { token: Token::NumberLiteral(n), .. }) if *n > 0.0 => {
                let n = *n;
                self.advance();
                n
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "a positive age like 90d, 6h, or 30m".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: None,
                });
            }
        };
        let min_age_days = match self.peek() {
            Some(SpannedToken { token: Token::Ident(u), .. }) => {
                let unit = u.to_ascii_lowercase();
                let days = match unit.as_str() {
                    "d" => n,
                    "h" => n / 24.0,
                    "m" => n / 1440.0,
                    _ => {
                        return Err(CalError::UnexpectedToken {
                            expected: "an age unit: d, h, or m".into(),
                            found: u.clone(),
                            span: self.peek().map(|t| t.span),
                            suggestion: None,
                        });
                    }
                };
                self.advance();
                days
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "an age unit: d, h, or m".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: Some("write the age as one word, e.g. 90d".into()),
                });
            }
        };

        // Optional TYPE <grain-type>, IN "<ns>", LIMIT <n> — any order.
        let mut grain_type: Option<String> = None;
        let mut namespace: Option<String> = None;
        let mut limit: Option<usize> = None;
        loop {
            match self.peek() {
                Some(SpannedToken { token: Token::Ident(w), .. })
                    if w.eq_ignore_ascii_case("TYPE") && grain_type.is_none() =>
                {
                    self.advance();
                    match self.peek() {
                        Some(SpannedToken { token: Token::Ident(t), .. }) => {
                            grain_type = Some(t.to_ascii_lowercase());
                            self.advance();
                        }
                        other => {
                            return Err(CalError::UnexpectedToken {
                                expected: "a grain type, e.g. event".into(),
                                found: other
                                    .map(|t| t.token.description())
                                    .unwrap_or_else(|| "end of input".into()),
                                span: other.map(|t| t.span),
                                suggestion: None,
                            });
                        }
                    }
                }
                Some(SpannedToken { token: Token::In, .. }) if namespace.is_none() => {
                    self.advance();
                    namespace = Some(self.parse_string_literal()?);
                }
                Some(SpannedToken { token: Token::Limit, .. }) if limit.is_none() => {
                    self.advance();
                    match self.peek() {
                        Some(SpannedToken { token: Token::NumberLiteral(n), .. })
                            if *n >= 1.0 =>
                        {
                            limit = Some(*n as usize);
                            self.advance();
                        }
                        other => {
                            return Err(CalError::UnexpectedToken {
                                expected: "a positive LIMIT".into(),
                                found: other
                                    .map(|t| t.token.description())
                                    .unwrap_or_else(|| "end of input".into()),
                                span: other.map(|t| t.span),
                                suggestion: None,
                            });
                        }
                    }
                }
                _ => break,
            }
        }

        // BECAUSE is mandatory — a retention sweep without a recorded reason
        // is not auditable.
        if !self.at_exact(&Token::Reason) && !self.at_exact(&Token::Because) {
            return Err(CalError::MissingReason {
                span: Some(self.current_span()),
            });
        }
        self.advance();
        let reason = self.parse_string_literal()?;

        let span_end = self.prev_span();
        Ok(CalStatement::Purge(PurgeStmt {
            min_age_days: Some(min_age_days),
            namespace,
            limit,
            grain_type,
            reason: Some(reason),
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// One DCL verb: an identifier, optionally dotted (`loop.review` lexes
    /// as Ident Dot Ident and is rejoined here). Validation against the
    /// verb registry happens at execution.
    fn parse_dcl_verb(&mut self) -> CalResult<String> {
        let head = match self.peek() {
            Some(SpannedToken { token: Token::Ident(w), .. }) => {
                let w = w.to_ascii_lowercase();
                self.advance();
                w
            }
            // RUN is a keyword token; the runtime verbs (`run.execute`,
            // `run.respond`, `run.cancel`) must still be grantable — a
            // driver that enforces run.respond while GRANT cannot spell it
            // is an approval boundary nobody can be admitted through.
            Some(SpannedToken { token: Token::Run, .. }) => {
                self.advance();
                "run".to_string()
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "a verb (read, write, supersede, delete, erase, \
                               loop.run, loop.review, loop.apply, run.execute, \
                               run.respond, run.cancel, admin)"
                        .into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: None,
                });
            }
        };
        if self.at_exact(&Token::Dot) {
            self.advance();
            match self.peek() {
                Some(SpannedToken { token: Token::Ident(tail), .. }) => {
                    let tail = tail.to_ascii_lowercase();
                    self.advance();
                    Ok(format!("{head}.{tail}"))
                }
                other => Err(CalError::UnexpectedToken {
                    expected: "the dotted verb tail (run, review, apply, \
                               execute, respond, cancel)"
                        .into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: None,
                }),
            }
        } else {
            Ok(head)
        }
    }

    /// The shared body of GRANT/REVOKE:
    /// `<verb>[, <verb>…] ON <ns|*>[, <ns>…] <TO|FROM> "<principal>"
    /// [WITH because("<why>")]`.
    fn parse_dcl_body(&mut self, recipient_token: &Token) -> CalResult<DclBody> {
        let mut verbs = vec![self.parse_dcl_verb()?];
        while self.at_exact(&Token::Comma) {
            self.advance();
            verbs.push(self.parse_dcl_verb()?);
        }

        self.expect_exact(&Token::On)?;
        let mut namespaces = Vec::new();
        loop {
            match self.peek() {
                Some(SpannedToken { token: Token::Asterisk, .. }) => {
                    namespaces.push("*".to_string());
                    self.advance();
                }
                Some(SpannedToken { token: Token::Ident(ns), .. }) => {
                    namespaces.push(ns.clone());
                    self.advance();
                }
                Some(SpannedToken { token: Token::StringLiteral(ns), .. }) => {
                    namespaces.push(ns.clone());
                    self.advance();
                }
                other => {
                    return Err(CalError::UnexpectedToken {
                        expected: "a namespace or *".into(),
                        found: other
                            .map(|t| t.token.description())
                            .unwrap_or_else(|| "end of input".into()),
                        span: other.map(|t| t.span),
                        suggestion: None,
                    });
                }
            }
            if self.at_exact(&Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        self.expect_exact(recipient_token)?;
        // Principals are string literals: they carry `:` and often `-`,
        // which the identifier grammar does not.
        let principal = self.parse_string_literal()?;

        // Optional `WITH because("<why>")` — BECAUSE is a keyword token, so
        // match it as one (the REASON alias works too).
        let mut reason = None;
        if self.at_exact(&Token::With) {
            self.advance();
            if self.at_exact(&Token::Because) || self.at_exact(&Token::Reason) {
                self.advance();
                self.expect_exact(&Token::LParen)?;
                reason = Some(self.parse_string_literal()?);
                self.expect_exact(&Token::RParen)?;
            } else {
                let other = self.peek();
                return Err(CalError::UnexpectedToken {
                    expected: "because(\"<why>\")".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: Some(
                        "GRANT/REVOKE support exactly one option: \
                         WITH because(\"<why>\")"
                            .into(),
                    ),
                });
            }
        }
        Ok((verbs, namespaces, principal, reason))
    }

    /// Parse `GRANT <verbs> ON <ns> TO "<principal>" [WITH because("…")]`.
    fn parse_grant(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Grant)?;
        let (verbs, namespaces, principal, reason) = self.parse_dcl_body(&Token::To)?;
        let span_end = self.prev_span();
        Ok(CalStatement::Grant(GrantStmt {
            verbs,
            namespaces,
            principal,
            reason,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `REVOKE <verbs> ON <ns> FROM "<principal>" [WITH because("…")]`.
    fn parse_revoke(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Revoke)?;
        let (verbs, namespaces, principal, reason) = self.parse_dcl_body(&Token::From)?;
        let span_end = self.prev_span();
        Ok(CalStatement::Revoke(RevokeStmt {
            verbs,
            namespaces,
            principal,
            reason,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// The shared body of the four governance statements (CAL 1.3 §8.16):
    /// `<hash> BECAUSE "<why>"`. BECAUSE is mandatory — a parse error, so
    /// the reason requirement is syntax, not convention.
    fn parse_governance_body(&mut self, keyword: &Token) -> CalResult<GovernanceStmt> {
        let span_start = self.current_span();
        self.expect_exact(keyword)?;
        let hash = self.parse_hash_literal()?;
        if !self.at_exact(&Token::Because) && !self.at_exact(&Token::Reason) {
            return Err(CalError::MissingReason {
                span: Some(self.current_span()),
            });
        }
        self.advance();
        let reason = self.parse_string_literal()?;
        let span_end = self.prev_span();
        Ok(GovernanceStmt {
            hash,
            reason,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        })
    }

    /// Parse `RUN LOOP [FULL] [WITH min_new(N), if_stale("<dur>")]` — the
    /// caller has seen `RUN` followed by the ident `LOOP`.
    fn parse_run_loop(&mut self, span_start: Span) -> CalResult<CalStatement> {
        // Consume the LOOP ident.
        self.advance();
        let mut full_sweep = false;
        if let Some(SpannedToken { token: Token::Ident(w), .. }) = self.peek() {
            if w.eq_ignore_ascii_case("FULL") {
                full_sweep = true;
                self.advance();
            }
        }
        let mut min_new: Option<u64> = None;
        let mut if_stale_ms: Option<i64> = None;
        if self.at_exact(&Token::With) {
            self.advance();
            loop {
                match self.peek() {
                    Some(SpannedToken { token: Token::Ident(opt), .. })
                        if opt.eq_ignore_ascii_case("min_new") =>
                    {
                        self.advance();
                        self.expect_exact(&Token::LParen)?;
                        match self.peek() {
                            Some(SpannedToken { token: Token::NumberLiteral(n), .. })
                                if *n >= 0.0 =>
                            {
                                min_new = Some(*n as u64);
                                self.advance();
                            }
                            other => {
                                return Err(CalError::UnexpectedToken {
                                    expected: "a number".into(),
                                    found: other
                                        .map(|t| t.token.description())
                                        .unwrap_or_else(|| "end of input".into()),
                                    span: other.map(|t| t.span),
                                    suggestion: None,
                                });
                            }
                        }
                        self.expect_exact(&Token::RParen)?;
                    }
                    Some(SpannedToken { token: Token::Ident(opt), .. })
                        if opt.eq_ignore_ascii_case("if_stale") =>
                    {
                        self.advance();
                        self.expect_exact(&Token::LParen)?;
                        let dur = self.parse_string_literal()?;
                        if_stale_ms = Some(parse_stale_duration_ms(&dur).ok_or_else(|| {
                            CalError::UnexpectedToken {
                                expected: "a duration like \"6h\", \"90m\", or \"2d\"".into(),
                                found: dur.clone(),
                                span: Some(self.prev_span()),
                                suggestion: None,
                            }
                        })?);
                        self.expect_exact(&Token::RParen)?;
                    }
                    other => {
                        return Err(CalError::UnexpectedToken {
                            expected: "min_new(N) or if_stale(\"<dur>\")".into(),
                            found: other
                                .map(|t| t.token.description())
                                .unwrap_or_else(|| "end of input".into()),
                            span: other.map(|t| t.span),
                            suggestion: Some(
                                "RUN LOOP supports WITH min_new(N), if_stale(\"6h\") — model \
                                 and policy configuration is host-side, never statement text"
                                    .into(),
                            ),
                        });
                    }
                }
                if self.at_exact(&Token::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let span_end = self.prev_span();
        Ok(CalStatement::RunLoop(RunLoopStmt {
            full_sweep,
            min_new,
            if_stale_ms,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `REMEMBER "<content>" [WITH session("<id>"), role("<r>"),
    /// run("<id>")]`.
    fn parse_remember(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Remember)?;
        let content = self.parse_string_literal()?;
        let mut session_id = None;
        let mut role = None;
        let mut run_id = None;
        if self.at_exact(&Token::With) {
            self.advance();
            loop {
                let opt = match self.peek() {
                    Some(SpannedToken { token: Token::Ident(w), .. }) => w.to_ascii_lowercase(),
                    // RUN is a keyword token; `run("<id>")` must still work.
                    Some(SpannedToken { token: Token::Run, .. }) => "run".to_string(),
                    other => {
                        return Err(CalError::UnexpectedToken {
                            expected: "session(\"<id>\"), role(\"<r>\"), or run(\"<id>\")".into(),
                            found: other
                                .map(|t| t.token.description())
                                .unwrap_or_else(|| "end of input".into()),
                            span: other.map(|t| t.span),
                            suggestion: None,
                        });
                    }
                };
                match opt.as_str() {
                    "session" | "role" | "run" => {
                        self.advance();
                        self.expect_exact(&Token::LParen)?;
                        let value = self.parse_string_literal()?;
                        self.expect_exact(&Token::RParen)?;
                        match opt.as_str() {
                            "session" => session_id = Some(value),
                            "role" => role = Some(value),
                            _ => run_id = Some(value),
                        }
                    }
                    other => {
                        return Err(CalError::UnexpectedToken {
                            expected: "session(\"<id>\"), role(\"<r>\"), or run(\"<id>\")".into(),
                            found: other.to_string(),
                            span: self.peek().map(|t| t.span),
                            suggestion: Some(
                                "REMEMBER carries capture metadata only — fact extraction \
                                 is host configuration (areev remember --model …)"
                                    .into(),
                            ),
                        });
                    }
                }
                if self.at_exact(&Token::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let span_end = self.prev_span();
        Ok(CalStatement::Remember(RememberStmt {
            content,
            session_id,
            role,
            run_id,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// One optional `<IDENT>(<n>)`-free numeric suffix keyword, e.g.
    /// `LIMIT 50` / `DEPTH 3`, where the keyword is a bare word.
    fn parse_word_number(&mut self, word: &str) -> CalResult<Option<usize>> {
        let matches_word = match self.peek() {
            Some(SpannedToken { token: Token::Ident(w), .. }) => w.eq_ignore_ascii_case(word),
            Some(SpannedToken { token: Token::Limit, .. }) => word.eq_ignore_ascii_case("LIMIT"),
            _ => false,
        };
        if !matches_word {
            return Ok(None);
        }
        self.advance();
        match self.peek() {
            Some(SpannedToken { token: Token::NumberLiteral(n), .. }) if *n >= 1.0 => {
                let n = *n as usize;
                self.advance();
                Ok(Some(n))
            }
            other => Err(CalError::UnexpectedToken {
                expected: format!("a positive number after {word}"),
                found: other
                    .map(|t| t.token.description())
                    .unwrap_or_else(|| "end of input".into()),
                span: other.map(|t| t.span),
                suggestion: None,
            }),
        }
    }

    /// Expect a specific bare-word identifier (case-insensitive).
    fn expect_word(&mut self, word: &str, form: &str) -> CalResult<()> {
        match self.peek() {
            Some(SpannedToken { token: Token::Ident(w), .. })
                if w.eq_ignore_ascii_case(word) =>
            {
                self.advance();
                Ok(())
            }
            other => Err(CalError::UnexpectedToken {
                expected: word.into(),
                found: other
                    .map(|t| t.token.description())
                    .unwrap_or_else(|| "end of input".into()),
                span: other.map(|t| t.span),
                suggestion: Some(form.into()),
            }),
        }
    }

    /// Parse `MERGE "<subject>" RELATION "<relation>" TO "<object>"
    /// [CONFIDENCE <n>] BECAUSE "<why>"` — close an open fork.
    fn parse_merge(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Merge)?;
        const FORM: &str = "a merge is spelled MERGE \"<subject>\" RELATION \"<relation>\" \
                            TO \"<object>\" [CONFIDENCE <n>] BECAUSE \"<why>\"";
        let subject = self.parse_string_literal()?;
        self.expect_word("RELATION", FORM)?;
        let relation = self.parse_string_literal()?;
        self.expect_exact(&Token::To)?;
        let object = self.parse_string_literal()?;
        let mut confidence = None;
        if let Some(SpannedToken { token: Token::Ident(w), .. }) = self.peek() {
            if w.eq_ignore_ascii_case("CONFIDENCE") {
                self.advance();
                match self.peek() {
                    Some(SpannedToken { token: Token::NumberLiteral(n), .. })
                        if (0.0..=1.0).contains(n) =>
                    {
                        confidence = Some(*n);
                        self.advance();
                    }
                    other => {
                        return Err(CalError::UnexpectedToken {
                            expected: "a confidence between 0 and 1".into(),
                            found: other
                                .map(|t| t.token.description())
                                .unwrap_or_else(|| "end of input".into()),
                            span: other.map(|t| t.span),
                            suggestion: None,
                        });
                    }
                }
            }
        }
        // Every write carries its reason; a fork resolution doubly so.
        if !self.at_exact(&Token::Reason) && !self.at_exact(&Token::Because) {
            return Err(CalError::MissingReason { span: Some(self.current_span()) });
        }
        self.advance();
        let reason = self.parse_string_literal()?;
        let span_end = self.prev_span();
        Ok(CalStatement::Merge(MergeStmt {
            subject,
            relation,
            object,
            confidence,
            reason,
            span: Some(Span::new(span_start.start, span_end.end, span_start.line, span_start.col)),
        }))
    }

    /// Parse `RELATED "<start>" VIA "<r1,r2>" [DIRECTION out|in|both]
    /// [DEPTH <n>] [LIMIT <n>]` — the bounded graph walk.
    fn parse_related(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Related)?;
        const FORM: &str = "the graph walk is spelled RELATED \"<start>\" VIA \
                            \"<r1,r2>\" [DIRECTION out|in|both] [DEPTH <n>] [LIMIT <n>]";
        let start = self.parse_string_literal()?;
        self.expect_word("VIA", FORM)?;
        let relations = self.parse_string_literal()?;
        let mut direction = None;
        if let Some(SpannedToken { token: Token::Ident(w), .. }) = self.peek() {
            if w.eq_ignore_ascii_case("DIRECTION") {
                self.advance();
                match self.peek() {
                    Some(SpannedToken { token: Token::Ident(d), .. })
                        if ["out", "in", "both"].iter().any(|x| d.eq_ignore_ascii_case(x)) =>
                    {
                        direction = Some(d.to_ascii_lowercase());
                        self.advance();
                    }
                    // IN doubles as the list-membership keyword token.
                    Some(SpannedToken { token: Token::In, .. }) => {
                        direction = Some("in".to_string());
                        self.advance();
                    }
                    other => {
                        return Err(CalError::UnexpectedToken {
                            expected: "out, in, or both".into(),
                            found: other
                                .map(|t| t.token.description())
                                .unwrap_or_else(|| "end of input".into()),
                            span: other.map(|t| t.span),
                            suggestion: None,
                        });
                    }
                }
            }
        }
        let depth = self.parse_word_number("DEPTH")?;
        let limit = self.parse_word_number("LIMIT")?;
        let span_end = self.prev_span();
        Ok(CalStatement::Related(RelatedStmt {
            start,
            relations,
            direction,
            depth,
            limit,
            span: Some(Span::new(span_start.start, span_end.end, span_start.line, span_start.col)),
        }))
    }

    /// Parse `NOVELTY "<text>" [SUBJECT "<s>"] [RELATION "<r>"]
    /// [LIMIT <k>]` — the paraphrase check.
    fn parse_novelty(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Novelty)?;
        let text = self.parse_string_literal()?;
        let mut subject = None;
        let mut relation = None;
        loop {
            match self.peek() {
                Some(SpannedToken { token: Token::Ident(w), .. })
                    if w.eq_ignore_ascii_case("SUBJECT") && subject.is_none() =>
                {
                    self.advance();
                    subject = Some(self.parse_string_literal()?);
                }
                Some(SpannedToken { token: Token::Ident(w), .. })
                    if w.eq_ignore_ascii_case("RELATION") && relation.is_none() =>
                {
                    self.advance();
                    relation = Some(self.parse_string_literal()?);
                }
                _ => break,
            }
        }
        let limit = self.parse_word_number("LIMIT")?;
        let span_end = self.prev_span();
        Ok(CalStatement::Novelty(NoveltyStmt {
            text,
            subject,
            relation,
            limit,
            span: Some(Span::new(span_start.start, span_end.end, span_start.line, span_start.col)),
        }))
    }

    /// Parse `ENTITY "<subject>" RELATION "<relation>" AT <epoch-ms>
    /// [AXIS world|knowledge]` (CAL 1.3 Wave 2 — the as-of read).
    fn parse_entity_at(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Entity)?;
        let subject = self.parse_string_literal()?;
        match self.peek() {
            Some(SpannedToken { token: Token::Ident(w), .. })
                if w.eq_ignore_ascii_case("RELATION") =>
            {
                self.advance();
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "RELATION".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: Some(
                        "the as-of read is spelled ENTITY \"<subject>\" RELATION \
                         \"<relation>\" AT <epoch-ms> [AXIS world|knowledge]"
                            .into(),
                    ),
                });
            }
        }
        let relation = self.parse_string_literal()?;
        match self.peek() {
            Some(SpannedToken { token: Token::Ident(w), .. }) if w.eq_ignore_ascii_case("AT") => {
                self.advance();
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "AT".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: None,
                });
            }
        }
        let at_ms = match self.peek() {
            Some(SpannedToken { token: Token::NumberLiteral(n), .. }) if *n >= 0.0 => {
                let n = *n as i64;
                self.advance();
                n
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "a point in time (epoch milliseconds)".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: None,
                });
            }
        };
        let mut axis = None;
        if let Some(SpannedToken { token: Token::Ident(w), .. }) = self.peek() {
            if w.eq_ignore_ascii_case("AXIS") {
                self.advance();
                match self.peek() {
                    Some(SpannedToken { token: Token::Ident(a), .. })
                        if a.eq_ignore_ascii_case("world") || a.eq_ignore_ascii_case("knowledge") =>
                    {
                        axis = Some(a.to_ascii_lowercase());
                        self.advance();
                    }
                    // KNOWLEDGE doubles as the relation-category keyword, so
                    // it arrives as its own token, not an Ident.
                    Some(SpannedToken { token: Token::Knowledge, .. }) => {
                        axis = Some("knowledge".to_string());
                        self.advance();
                    }
                    other => {
                        return Err(CalError::UnexpectedToken {
                            expected: "world or knowledge".into(),
                            found: other
                                .map(|t| t.token.description())
                                .unwrap_or_else(|| "end of input".into()),
                            span: other.map(|t| t.span),
                            suggestion: None,
                        });
                    }
                }
            }
        }
        let span_end = self.prev_span();
        Ok(CalStatement::EntityAt(EntityAtStmt {
            subject,
            relation,
            at_ms,
            axis,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `RUN TRACE "<run-id>" [LIMIT <n>]` — the caller has seen
    /// `RUN` followed by the ident `TRACE`.
    fn parse_run_trace(&mut self, span_start: Span) -> CalResult<CalStatement> {
        self.advance(); // TRACE
        let run_id = self.parse_string_literal()?;
        let limit = self.parse_word_number("LIMIT")?;
        let span_end = self.prev_span();
        Ok(CalStatement::RunTrace(RunTraceStmt {
            run_id,
            limit,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `RUNS TOUCHING <hash> [DEPTH <n>]`.
    fn parse_runs_touching(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Runs)?;
        match self.peek() {
            Some(SpannedToken { token: Token::Ident(w), .. })
                if w.eq_ignore_ascii_case("TOUCHING") =>
            {
                self.advance();
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "TOUCHING".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: Some("the reverse join is spelled RUNS TOUCHING <hash> [DEPTH <n>]".into()),
                });
            }
        }
        let hash = self.parse_hash_literal()?;
        let depth = self.parse_word_number("DEPTH")?;
        let span_end = self.prev_span();
        Ok(CalStatement::RunsTouching(RunsTouchingStmt {
            hash,
            depth,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `DERIVED FROM <hash>` — reverse provenance.
    fn parse_derived_from(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Derived)?;
        self.expect_exact(&Token::From)?;
        let hash = self.parse_hash_literal()?;
        let span_end = self.prev_span();
        Ok(CalStatement::DerivedFrom(DerivedFromStmt {
            hash,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse `SHOW GRANTS [FOR "<principal>"]`.
    fn parse_show_grants(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Show)?;
        match self.peek() {
            Some(SpannedToken { token: Token::Ident(w), .. })
                if w.eq_ignore_ascii_case("GRANTS") =>
            {
                self.advance();
            }
            Some(SpannedToken { token: Token::Ident(w), .. })
                if w.eq_ignore_ascii_case("FORKS") =>
            {
                self.advance();
                let span_end = self.prev_span();
                return Ok(CalStatement::ShowForks(ShowForksStmt {
                    span: Some(Span::new(
                        span_start.start,
                        span_end.end,
                        span_start.line,
                        span_start.col,
                    )),
                }));
            }
            other => {
                return Err(CalError::UnexpectedToken {
                    expected: "GRANTS or FORKS".into(),
                    found: other
                        .map(|t| t.token.description())
                        .unwrap_or_else(|| "end of input".into()),
                    span: other.map(|t| t.span),
                    suggestion: Some("SHOW GRANTS [FOR \"<principal>\"] | SHOW FORKS".into()),
                });
            }
        }
        let principal = if self.at_exact(&Token::For) {
            self.advance();
            Some(self.parse_string_literal()?)
        } else {
            None
        };
        let span_end = self.prev_span();
        Ok(CalStatement::ShowGrants(ShowGrantsStmt {
            principal,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    fn parse_revert(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Revert)?;

        let hash = self.parse_hash_literal()?;

        // REASON / BECAUSE is required.
        if !self.at_exact(&Token::Reason) && !self.at_exact(&Token::Because) {
            return Err(CalError::MissingReason {
                span: Some(self.current_span()),
            });
        }
        self.advance();
        let rspan = self.current_span();
        let reason = self.parse_string_literal()?;
        if reason.len() > MAX_REASON_LENGTH {
            return Err(CalError::ReasonTooLong {
                length: reason.len(),
                max: MAX_REASON_LENGTH,
                span: Some(rspan),
            });
        }

        let span_end = self.prev_span();
        Ok(CalStatement::Revert(RevertStmt {
            hash,
            reason,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    // -- DROP (Tier 2) ----------------------------------------------------

    fn parse_drop(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Drop)?;

        if self.at_exact(&Token::Template) {
            self.advance(); // consume TEMPLATE
            let name = self.parse_string_literal()?;
            let span_end = self.prev_span();
            Ok(CalStatement::DropTemplate(super::ast::DropTemplateStmt {
                name,
                span: Some(Span::new(
                    span_start.start,
                    span_end.end,
                    span_start.line,
                    span_start.col,
                )),
            }))
        } else if self.at_exact(&Token::Query) {
            self.advance(); // consume QUERY
            let name = self.parse_string_literal()?;
            let span_end = self.prev_span();
            Ok(CalStatement::DropQuery(DropQueryStmt {
                name,
                span: Some(Span::new(
                    span_start.start,
                    span_end.end,
                    span_start.line,
                    span_start.col,
                )),
            }))
        } else {
            Err(CalError::UnexpectedToken {
                expected: "TEMPLATE or QUERY after DROP".into(),
                found: self
                    .peek()
                    .map(|t| t.token.description())
                    .unwrap_or("end of input".into()),
                span: Some(self.current_span()),
                suggestion: Some("Use DROP TEMPLATE \"name\" or DROP QUERY \"name\".".into()),
            })
        }
    }

    // -- DEFINE dispatcher -------------------------------------------------

    /// Dispatch DEFINE to either DEFINE TEMPLATE or DEFINE QUERY.
    fn parse_define(&mut self) -> CalResult<CalStatement> {
        // Peek at the token after DEFINE to determine which variant.
        let next = self.tokens.get(self.pos + 1);
        match next {
            Some(SpannedToken {
                token: Token::Template,
                ..
            }) => self.parse_define_template(),
            Some(SpannedToken {
                token: Token::Query,
                ..
            }) => self.parse_define_query(),
            _ => Err(CalError::UnexpectedToken {
                expected: "TEMPLATE or QUERY after DEFINE".into(),
                found: next
                    .map(|t| t.token.description())
                    .unwrap_or("end of input".into()),
                span: Some(self.current_span()),
                suggestion: Some("Use DEFINE TEMPLATE or DEFINE QUERY.".into()),
            }),
        }
    }

    // -- DEFINE TEMPLATE --------------------------------------------------

    /// Parse `DEFINE TEMPLATE "name" [DESCRIPTION "..."] [EXTENDS "parent"] [FOR grain_types] AS "source"`.
    fn parse_define_template(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Define)?;
        self.expect_exact(&Token::Template)?;

        // §7 spells the name as a bare identifier. A quoted name is still
        // accepted — it was the only form Areev ever wrote, and it is the
        // only way to name a template containing a space.
        let name = self.parse_template_name()?;

        let mut description = None;
        let mut parent = None;
        let mut grain_types = Vec::new();

        // Parse optional clauses before AS.
        loop {
            match self.peek() {
                Some(SpannedToken {
                    token: Token::Ident(s),
                    ..
                }) if s.eq_ignore_ascii_case("DESCRIPTION") => {
                    self.advance();
                    description = Some(self.parse_string_literal()?);
                }
                Some(SpannedToken {
                    token: Token::Extends,
                    ..
                }) => {
                    self.advance();
                    parent = Some(self.parse_template_name()?);
                }
                Some(SpannedToken {
                    token: Token::For, ..
                }) => {
                    self.advance();
                    // Parse comma-separated grain type names.
                    loop {
                        let gt = match self.peek() {
                            Some(SpannedToken {
                                token: Token::Ident(s),
                                ..
                            }) => {
                                let s = s.clone();
                                self.advance();
                                s
                            }
                            Some(st) => {
                                let desc = st.token.description();
                                // Allow keywords that are also grain type names.
                                self.advance();
                                desc
                            }
                            None => break,
                        };
                        grain_types.push(gt.to_lowercase());
                        if self.at_exact(&Token::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }

        // Body: either the `AS "<text>"` shorthand (§10.6.1) or a section
        // list (§10.6). Exactly one — a definition may not combine them.
        let (source, sections) = if self.at_exact(&Token::As) {
            self.advance();
            let source_span = self.current_span();
            let source = self.parse_string_literal()?;
            if source.is_empty() {
                return Err(CalError::TemplateSyntaxError {
                    detail: "template source cannot be empty".to_string(),
                    span: Some(source_span),
                });
            }
            // §10.6.1: `AS "<text>"` is exactly `ELEMENT { <text> }`, so it
            // desugars here and the rest of the pipeline only ever sees
            // sections.
            //
            // Compatibility shim: a body that drives its own iteration with
            // `{{#each grains}}` is a pre-§10.6 whole-result template. That
            // construct is meaningless inside an ELEMENT body (the engine
            // already supplies the grain) and reinterpreting it would render
            // the whole result set once per grain, so those bodies keep the
            // old semantics. Remove once no such templates remain in the wild.
            if source.contains("{{#each") {
                (source, None)
            } else {
                let sections = TemplateSectionSources {
                    element: Some(source),
                    ..Default::default()
                };
                (sections.to_source(), Some(sections))
            }
        } else {
            let sections = self.parse_template_sections()?;
            if sections.is_empty() {
                return Err(CalError::TemplateSyntaxError {
                    detail: "template body must be AS \"...\" or at least one \
                             section (HEADER, ELEMENT, ELEMENT_SUMMARY, \
                             ELEMENT_OMIT, SOURCE_BREAK, FOOTER)"
                        .to_string(),
                    span: Some(self.current_span()),
                });
            }
            (sections.to_source(), Some(sections))
        };

        let span_end = self.prev_span();
        Ok(CalStatement::DefineTemplate(DefineTemplateStmt {
            name,
            description,
            parent,
            grain_types,
            source,
            sections,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse a run of `HEADER { ... } ELEMENT { ... } ...` sections (§10.6).
    ///
    /// Bodies arrive from the lexer already captured as raw text — see
    /// [`super::lexer::SectionBody`] — so nothing here has to cope with
    /// template prose that is not valid CAL.
    fn parse_template_sections(&mut self) -> CalResult<TemplateSectionSources> {
        let mut out = TemplateSectionSources::default();

        loop {
            let span = self.current_span();
            let Some(st) = self.peek() else { break };
            let (keyword, body) = match &st.token {
                Token::Header(b) => ("HEADER", b),
                Token::Element(b) => ("ELEMENT", b),
                Token::ElementSummary(b) => ("ELEMENT_SUMMARY", b),
                Token::ElementOmit(b) => ("ELEMENT_OMIT", b),
                Token::SourceBreak(b) => ("SOURCE_BREAK", b),
                Token::Footer(b) => ("FOOTER", b),
                _ => break,
            };
            // A bare keyword is an ordinary word here, not a section.
            let SectionBody::Body(text) = body else { break };
            let text = text.clone();
            self.advance();

            let slot = match keyword {
                "HEADER" => &mut out.header,
                "ELEMENT" => &mut out.element,
                "ELEMENT_SUMMARY" => &mut out.element_summary,
                "ELEMENT_OMIT" => &mut out.element_omit,
                "SOURCE_BREAK" => &mut out.source_break,
                _ => &mut out.footer,
            };
            Self::set_section(slot, text, keyword, span)?;
        }

        Ok(out)
    }

    /// Parse a template name: a bare identifier (§7) or a quoted string.
    ///
    /// `template_name = identifier` in the spec. The quoted form stays
    /// accepted because it is what Areev has always written and the only way
    /// to spell a name containing a space; in `FORMAT` position the two are
    /// deliberately *not* interchangeable — there a quoted argument is a
    /// template body, never a name.
    fn parse_template_name(&mut self) -> CalResult<String> {
        if matches!(
            self.peek(),
            Some(SpannedToken {
                token: Token::StringLiteral(_),
                ..
            })
        ) {
            return self.parse_string_literal();
        }

        let span = self.current_span();
        // `AS` here means the name was omitted; without this guard the label
        // parser would happily swallow it as the name.
        if self.at_exact(&Token::As) {
            return Err(CalError::UnexpectedToken {
                expected: "a template name".into(),
                found: "AS".into(),
                span: Some(span),
                suggestion: Some("Name the template: DEFINE TEMPLATE <name> AS \"...\"".into()),
            });
        }

        // `parse_label`, not `parse_identifier`: preset names (`structured`,
        // `compact`, …) are keyword tokens, and `EXTENDS structured` has to
        // parse.
        let name = self.parse_label()?;
        // Defence in depth (invariant 3): a template name is inert, but the
        // destructive vocabulary stays out of CAL text everywhere it can
        // appear.
        if is_destructive_keyword(&name) {
            return Err(CalError::UnexpectedToken {
                expected: "a template name".into(),
                found: name,
                span: Some(span),
                suggestion: Some(
                    "Choose a different name — CAL keeps destructive words out \
                     of its grammar entirely."
                        .into(),
                ),
            });
        }
        Ok(name)
    }

    /// Assign a section body, rejecting a second definition of the same one.
    fn set_section(
        slot: &mut Option<String>,
        text: String,
        keyword: &str,
        span: Span,
    ) -> CalResult<()> {
        if slot.is_some() {
            return Err(CalError::TemplateSyntaxError {
                detail: format!("section {keyword} is defined twice"),
                span: Some(span),
            });
        }
        *slot = Some(text);
        Ok(())
    }

    // -- DEFINE QUERY -----------------------------------------------------

    /// Parse `DEFINE QUERY "name" [($param [= default], ...)] [DESCRIPTION "..."] AS { body }`.
    fn parse_define_query(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Define)?;
        self.expect_exact(&Token::Query)?;

        // Parse query name (string literal).
        let name = self.parse_string_literal()?;

        // Validate name format (reuse template name regex).
        if !crate::queries::QueryRegistry::is_valid_name(&name) {
            return Err(CalError::TemplateInvalidName {
                name,
                span: Some(self.current_span()),
            });
        }

        // Optional parameter declarations: ($param1, $param2 = default, ...)
        let params = if self.at_exact(&Token::LParen) {
            self.parse_query_param_decls()?
        } else {
            Vec::new()
        };

        // Validate param count.
        if params.len() > crate::queries::MAX_QUERY_PARAMS {
            return Err(CalError::TooManyQueryParams {
                count: params.len(),
                max: crate::queries::MAX_QUERY_PARAMS,
                span: Some(self.current_span()),
            });
        }

        // Optional DESCRIPTION clause.
        let description = if matches!(
            self.peek(),
            Some(SpannedToken {
                token: Token::Ident(s),
                ..
            }) if s.eq_ignore_ascii_case("DESCRIPTION")
        ) {
            self.advance();
            Some(self.parse_string_literal()?)
        } else {
            None
        };

        // AS keyword.
        self.expect_exact(&Token::As)?;

        // Parse body: { ... }
        self.expect_exact(&Token::LBrace)?;
        let body_start = self.pos;

        // Collect all tokens until matching closing brace.
        let mut brace_depth = 1u32;
        while brace_depth > 0 {
            match self.peek() {
                Some(SpannedToken {
                    token: Token::LBrace,
                    ..
                }) => {
                    brace_depth += 1;
                    self.advance();
                }
                Some(SpannedToken {
                    token: Token::RBrace,
                    ..
                }) => {
                    brace_depth -= 1;
                    if brace_depth > 0 {
                        self.advance();
                    }
                }
                None => {
                    return Err(CalError::UnexpectedToken {
                        expected: "closing '}' for DEFINE QUERY body".into(),
                        found: "end of input".into(),
                        span: Some(self.current_span()),
                        suggestion: None,
                    });
                }
                _ => {
                    self.advance();
                }
            }
        }

        // Reconstruct body text from tokens between braces.
        let body_text = self.reconstruct_body_text(body_start);

        // Validate body size.
        if body_text.len() > crate::queries::MAX_QUERY_BODY_SIZE {
            return Err(CalError::QueryBodyTooLarge {
                size: body_text.len(),
                max: crate::queries::MAX_QUERY_BODY_SIZE,
                span: Some(span_start),
            });
        }

        // Validate body contains no write-tier or recursive statements.
        // We do a quick parse of the body to check.
        self.validate_query_body(&body_text, &span_start)?;

        self.advance(); // consume closing }

        let span_end = self.prev_span();
        Ok(CalStatement::DefineQuery(DefineQueryStmt {
            name,
            description,
            params,
            body: body_text,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse parameter declarations: `($name [= default], ...)`.
    fn parse_query_param_decls(&mut self) -> CalResult<Vec<QueryParam>> {
        self.expect_exact(&Token::LParen)?;
        let mut params = Vec::new();

        loop {
            if self.at_exact(&Token::RParen) {
                self.advance();
                break;
            }
            if !params.is_empty() {
                self.expect_exact(&Token::Comma)?;
            }

            // Expect $name (lexed as Token::Parameter("name"))
            let name = match self.peek() {
                Some(SpannedToken {
                    token: Token::Parameter(n),
                    ..
                }) => {
                    let n = n.clone();
                    self.advance();
                    n
                }
                _ => {
                    return Err(CalError::UnexpectedToken {
                        expected: "parameter name ($name)".into(),
                        found: self
                            .peek()
                            .map(|t| t.token.description())
                            .unwrap_or("end of input".into()),
                        span: Some(self.current_span()),
                        suggestion: Some(
                            "Parameter declarations use $name syntax, e.g. ($user, $limit = 10)"
                                .into(),
                        ),
                    });
                }
            };

            // Optional = default
            let default = if self.at_exact(&Token::Eq) {
                self.advance();
                Some(self.parse_value()?)
            } else {
                None
            };

            params.push(QueryParam { name, default });
        }

        Ok(params)
    }

    /// Reconstruct body text from the source input between token positions.
    fn reconstruct_body_text(&self, body_start: usize) -> String {
        // Get the byte range from the first token after { to the token before }
        if body_start >= self.pos {
            return String::new();
        }
        let start_byte = self.tokens[body_start].span.start;
        let end_byte = self.tokens[self.pos - 1].span.end;
        self.input[start_byte..end_byte].trim().to_string()
    }

    /// Validate that a query body contains only read-tier statements, and that
    /// it can actually parse.
    fn validate_query_body(&self, body: &str, span: &Span) -> CalResult<()> {
        // A `$`-bearing body still gets the evade-proof keyword scan below, and
        // then a real parse. It used to get ONLY the keyword scan, so any
        // syntax error in a parameterized body — the shape most saved queries
        // have — was stored unverified and first surfaced when a caller ran it.
        // A stored query that can never run is a landmine, and its first caller
        // is often an unattended agent.
        if body.contains('$') {
            // Word-level lexical scan (a `$`-body can't fully parse until RUN
            // substitutes params, so this guard must not be evadable). Split on
            // anything that isn't an identifier char so ANY whitespace or
            // punctuation between a keyword and its operand counts as a boundary
            // — `SUPERSEDE\n` no longer slips past a `"SUPERSEDE "` substring
            // test. Covers DROP/DEFINE (and the lexer-blocked destructive words)
            // that the old list missed. RUN is re-checked precisely at execution
            // via check_read_only_statement; this is only the DEFINE-time guard.
            let upper = body.to_ascii_uppercase();
            for word in upper.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
                if word == "RUN" {
                    return Err(CalError::RecursiveQuery { span: Some(*span) });
                }
                if matches!(
                    word,
                    "ADD" | "SUPERSEDE" | "ACCUMULATE" | "REVERT" | "FORGET" | "PURGE" | "DROP"
                        | "DEFINE" | "DELETE" | "ERASE" | "TRUNCATE" | "INSERT" | "UPDATE"
                        | "CREATE" | "GRANT"
                ) {
                    return Err(CalError::WriteInQueryBody {
                        stmt: word.to_string(),
                        span: Some(*span),
                    });
                }
            }
            // Now the shape. The reason the parse used to be skipped here is
            // real but narrow: a parameter in a NUMERIC position (`RECENT
            // $limit`) is not a valid number literal until RUN substitutes it,
            // so the body as written genuinely does not parse. Value positions
            // (`WHERE x = $p`) do. So try it as written, then retry with every
            // parameter standing in as a literal — a body that fails BOTH is
            // malformed no matter what RUN eventually substitutes.
            match super::parser::parse(body) {
                Ok(q) => return self.check_statement_read_only(&q.statement, span),
                Err(strict) => {
                    let filled = params_as_literals(body);
                    match super::parser::parse(&filled) {
                        Ok(q) => return self.check_statement_read_only(&q.statement, span),
                        // Report the error from the body as the author wrote
                        // it; the placeholder form is an internal probe and its
                        // spans would point at text the author never typed.
                        Err(_) => {
                            return Err(CalError::InvalidQueryBody {
                                detail: strict.to_string(),
                                span: Some(*span),
                            })
                        }
                    }
                }
            }
        }

        // No parameters — safe to parse and validate fully.
        let parsed = match super::parser::parse(body) {
            Ok(q) => q,
            Err(e) => {
                return Err(CalError::InvalidQueryBody {
                    detail: e.to_string(),
                    span: Some(*span),
                });
            }
        };

        // Check statement type — reject write-tier and recursive.
        self.check_statement_read_only(&parsed.statement, span)?;
        Ok(())
    }

    /// Recursively check that a statement is read-only (no writes, no RUN).
    /// Delegates to the free [`check_read_only_statement`] so the executor can
    /// apply the identical gate to a parsed RUN body at execution time.
    fn check_statement_read_only(&self, stmt: &CalStatement, span: &Span) -> CalResult<()> {
        check_read_only_statement(stmt, span)
    }

    // -- RUN ---------------------------------------------------------------

    /// Parse `RUN "name" [($param = value, ...)]`.
    fn parse_run_query(&mut self) -> CalResult<CalStatement> {
        let span_start = self.current_span();
        self.expect_exact(&Token::Run)?;

        // `RUN LOOP` (governance) and `RUN TRACE` (the run↔memory join)
        // ride the RUN prefix; a string literal keeps meaning a saved query.
        if let Some(SpannedToken { token: Token::Ident(w), .. }) = self.peek() {
            if w.eq_ignore_ascii_case("LOOP") {
                return self.parse_run_loop(span_start);
            }
            if w.eq_ignore_ascii_case("TRACE") {
                return self.parse_run_trace(span_start);
            }
        }

        let name = self.parse_string_literal()?;

        // Optional parameter bindings: ($name = value, ...)
        let bindings = if self.at_exact(&Token::LParen) {
            self.parse_query_param_bindings()?
        } else {
            Vec::new()
        };

        let span_end = self.prev_span();
        Ok(CalStatement::RunQuery(RunQueryStmt {
            name,
            bindings,
            span: Some(Span::new(
                span_start.start,
                span_end.end,
                span_start.line,
                span_start.col,
            )),
        }))
    }

    /// Parse parameter bindings: `($name = value, ...)`.
    fn parse_query_param_bindings(&mut self) -> CalResult<Vec<(String, Value)>> {
        self.expect_exact(&Token::LParen)?;
        let mut bindings = Vec::new();

        loop {
            if self.at_exact(&Token::RParen) {
                self.advance();
                break;
            }
            if !bindings.is_empty() {
                self.expect_exact(&Token::Comma)?;
            }

            // $name = value (lexed as Token::Parameter("name"))
            let name = match self.peek() {
                Some(SpannedToken {
                    token: Token::Parameter(n),
                    ..
                }) => {
                    let n = n.clone();
                    self.advance();
                    n
                }
                _ => {
                    return Err(CalError::UnexpectedToken {
                        expected: "parameter binding ($name = value)".into(),
                        found: self
                            .peek()
                            .map(|t| t.token.description())
                            .unwrap_or("end of input".into()),
                        span: Some(self.current_span()),
                        suggestion: Some(
                            "Parameter bindings use $name = value syntax, e.g. ($user = \"john\")"
                                .into(),
                        ),
                    });
                }
            };
            self.expect_exact(&Token::Eq)?;
            let value = self.parse_value()?;
            bindings.push((name, value));
        }

        Ok(bindings)
    }

    // -- STREAM ASSEMBLE --------------------------------------------------

    /// Parse `STREAM ASSEMBLE ...` — delegates to `parse_assemble()` then sets streaming flag.
    fn parse_stream_assemble(&mut self) -> CalResult<CalStatement> {
        self.expect_exact(&Token::Stream)?;
        // parse_assemble() consumes the ASSEMBLE token and rest of the statement.
        let stmt = self.parse_assemble()?;
        match stmt {
            CalStatement::Assemble(mut asm) => {
                asm.streaming = true;
                Ok(CalStatement::Assemble(asm))
            }
            other => Ok(other), // Should not happen, but don't panic.
        }
    }
}

// ---------------------------------------------------------------------------
// Suggestion helpers
// ---------------------------------------------------------------------------

/// Return a human-readable suggestion for an unknown plural grain type string.
fn suggest_grain_type_plural(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    // Old OMS 1.1 names.
    match lower.as_str() {
        "beliefs" | "belief" => return Some("did you mean \"facts\"?".into()),
        "episodes" | "episode" => return Some("did you mean \"events\"?".into()),
        "checkpoints" | "checkpoint" => return Some("did you mean \"states\"?".into()),
        "actions" | "action" | "toolcalls" | "tool_calls" | "tool_call" => {
            return Some("did you mean \"tools\"?".into())
        }
        _ => {}
    }
    // NOTE: Singular grain type names (fact, event, reasoning, etc.) are now
    // accepted directly by `GrainTypePlural::parse()` and will never reach here.
    None
}

/// Return a suggestion for an unknown singular grain type string.
fn suggest_grain_type_singular(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "belief" => return Some("did you mean \"fact\"?".into()),
        "episode" => return Some("did you mean \"event\"?".into()),
        "checkpoint" => return Some("did you mean \"state\"?".into()),
        "toolcall" | "tool_call" => return Some("did you mean \"tool\"?".into()),
        _ => {}
    }
    None
}

// ---------------------------------------------------------------------------
// Re-export Span for the SinceClause import
// ---------------------------------------------------------------------------

use super::ast::SinceClause;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn p(input: &str) -> CalQuery {
        parse(input).expect(input)
    }

    fn pe(input: &str) -> CalError {
        parse(input).expect_err(input)
    }

    /// Shorthand to build an `AliasedFormat` without an alias.
    fn af(spec: FormatSpec) -> AliasedFormat {
        AliasedFormat { spec, alias: None }
    }

    /// Shorthand to build an `AliasedFormat` with an alias.
    fn af_as(spec: FormatSpec, alias: &str) -> AliasedFormat {
        AliasedFormat {
            spec,
            alias: Some(alias.to_string()),
        }
    }

    // ── 0. Whole-input consumption ────────────────────────────────────────

    #[test]
    fn test_trailing_garbage_rejected() {
        let e = pe(r#"RECALL facts WHERE subject = "john" banana banana"#);
        assert!(e.to_string().contains("end of query"), "{e}");
    }

    #[test]
    fn test_trailing_second_statement_rejected() {
        // A smuggled second statement must be an error, never silently dropped.
        let e = pe(r#"RECALL facts WHERE subject = "john"; RECALL facts WHERE subject = "mary""#);
        assert!(e.to_string().contains("end of query"), "{e}");
        let e = pe(r#"RECALL facts WHERE subject = "john"; DELETE FROM facts"#);
        assert!(e.to_string().contains("end of query"), "{e}");
    }

    #[test]
    fn test_trailing_semicolons_allowed() {
        p(r#"RECALL facts WHERE subject = "john";"#);
        p(r#"RECALL facts WHERE subject = "john";;"#);
    }

    #[test]
    fn test_assemble_with_mixed_dedup_list_keeps_all_options() {
        // `WITH dedup, <general options>` on ASSEMBLE: `dedup` stays
        // assemble-scoped, and the rest are kept on the AssembleStmt's OWN
        // `with_options` (not silently dropped, and not routed to the enclosing
        // query) so they scope to this assemble's recall.
        let q = p("ASSEMBLE \"ctx\" FROM a: (RECALL facts), b: (RECALL events) \
                   BUDGET 400 tokens FORMAT sml \
                   WITH dedup, recency_weight(0.7), provenance");
        let CalStatement::Assemble(a) = &q.statement else {
            panic!("expected Assemble, got {:?}", q.statement);
        };
        assert_eq!(a.assemble_with.len(), 1, "dedup stays assemble-scoped");
        assert!(
            a.with_options.iter().any(|o| matches!(o, WithOption::RecencyWeight { .. })),
            "recency_weight kept on the assemble: {:?}",
            a.with_options
        );
        assert!(
            a.with_options.iter().any(|o| matches!(o, WithOption::Provenance)),
            "provenance kept on the assemble: {:?}",
            a.with_options
        );
        assert!(
            q.with_options.is_empty(),
            "options stay assemble-scoped, not on the enclosing query: {:?}",
            q.with_options
        );
    }

    /// Regression (#A3F2): a NESTED ASSEMBLE's `WITH dedup, <opts>` options stay
    /// on the inner ASSEMBLE — they must not leak to the enclosing EXPLAIN /
    /// COALESCE query (the shared `pending_with_options` field used to bind them
    /// to the wrong statement when the ASSEMBLE was nested).
    #[test]
    fn test_nested_assemble_with_options_do_not_leak() {
        fn inner_assemble(stmt: &CalStatement) -> &AssembleStmt {
            match stmt {
                CalStatement::Assemble(a) => a,
                CalStatement::Explain(e) => inner_assemble(&e.inner),
                CalStatement::Coalesce(c) => inner_assemble(&c.branches[0].query),
                other => panic!("expected an ASSEMBLE (possibly wrapped), got {:?}", other),
            }
        }
        // FORMAT before WITH forces the trailing clause onto the assemble-level
        // path (otherwise the last source greedily consumes it).
        let src = "ASSEMBLE \"ctx\" FROM a: (RECALL facts), b: (RECALL events) \
                   FORMAT sml WITH dedup, query_expansion";
        for wrapped in [format!("EXPLAIN {src}"), format!("COALESCE {{ {src} }} OR {{ RECALL facts }}")] {
            let q = p(&wrapped);
            assert!(
                q.with_options.is_empty(),
                "{wrapped:?}: options leaked to the wrapper query: {:?}",
                q.with_options
            );
            let a = inner_assemble(&q.statement);
            assert!(
                a.with_options.iter().any(|o| matches!(o, WithOption::QueryExpansion)),
                "{wrapped:?}: query_expansion must scope to the inner assemble: {:?}",
                a.with_options
            );
        }
    }

    // ── 1. Simple RECALL ──────────────────────────────────────────────────

    #[test]
    fn test_recall_beliefs() {
        let q = p("RECALL facts");
        match &q.statement {
            CalStatement::Recall(r) => {
                assert_eq!(r.grain_type, GrainTypePlural::Facts);
                assert!(r.where_clause.is_none());
                assert!(r.about.is_none());
            }
            other => panic!("expected Recall, got {:?}", other),
        }
    }

    // ── 2. RECALL with WHERE ──────────────────────────────────────────────

    #[test]
    fn test_recall_where_subject_eq() {
        let q = p(r#"RECALL facts WHERE subject = "john""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                assert!(r.where_clause.is_some());
                let wc = r.where_clause.as_ref().unwrap();
                match &wc.condition {
                    Condition::Comparison {
                        field,
                        comparator,
                        value,
                        ..
                    } => {
                        assert_eq!(field, "subject");
                        assert_eq!(*comparator, Comparator::Eq);
                        assert_eq!(
                            *value,
                            Value::String {
                                value: "john".into()
                            }
                        );
                    }
                    other => panic!("expected Comparison, got {:?}", other),
                }
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 3. RECALL with ABOUT ──────────────────────────────────────────────

    #[test]
    fn test_recall_about() {
        let q = p(r#"RECALL facts ABOUT "john preferences""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                assert_eq!(r.about.as_ref().unwrap().text, "john preferences");
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 4. RECALL with RECENT ─────────────────────────────────────────────

    #[test]
    fn test_recall_recent() {
        let q = p("RECALL events RECENT 5");
        match &q.statement {
            CalStatement::Recall(r) => {
                assert_eq!(r.grain_type, GrainTypePlural::Events);
                assert_eq!(r.recent.as_ref().unwrap().count, 5);
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 5. RECALL with SINCE ──────────────────────────────────────────────

    #[test]
    fn test_recall_since() {
        let q = p(r#"RECALL facts SINCE "3 days ago""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                assert_eq!(r.since.as_ref().unwrap().expression, "3 days ago");
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 6. RECALL with pipeline ───────────────────────────────────────────

    #[test]
    fn test_recall_pipeline() {
        // Pipe-free syntax (canonical).
        let q = p(r#"RECALL facts WHERE subject = "john" ORDER BY confidence DESC LIMIT 10"#);
        assert_eq!(q.pipeline.len(), 2);
        match &q.pipeline[0] {
            PipelineStage::OrderBy {
                field, descending, ..
            } => {
                assert_eq!(field, "confidence");
                assert!(*descending);
            }
            other => panic!("expected OrderBy, got {:?}", other),
        }
        match &q.pipeline[1] {
            PipelineStage::Limit { value, .. } => assert_eq!(*value, 10),
            other => panic!("expected Limit, got {:?}", other),
        }
    }

    #[test]
    fn test_recall_pipeline_with_pipe_backward_compat() {
        // Legacy pipe syntax must still parse identically.
        let q = p(r#"RECALL facts WHERE subject = "john" | ORDER BY confidence DESC | LIMIT 10"#);
        assert_eq!(q.pipeline.len(), 2);
        match &q.pipeline[0] {
            PipelineStage::OrderBy {
                field, descending, ..
            } => {
                assert_eq!(field, "confidence");
                assert!(*descending);
            }
            other => panic!("expected OrderBy, got {:?}", other),
        }
        match &q.pipeline[1] {
            PipelineStage::Limit { value, .. } => assert_eq!(*value, 10),
            other => panic!("expected Limit, got {:?}", other),
        }
    }

    // ── 7. RECALL with WITH ───────────────────────────────────────────────

    #[test]
    fn test_recall_with_superseded_and_score_breakdown() {
        let q = p("RECALL facts WITH superseded, score_breakdown");
        assert!(q.with_options.contains(&WithOption::Superseded));
        assert!(q.with_options.contains(&WithOption::ScoreBreakdown));
    }

    // ── 8. RECALL with FORMAT ─────────────────────────────────────────────

    #[test]
    fn test_recall_format_json() {
        let q = p("RECALL facts FORMAT json");
        assert_eq!(q.format, Some(FormatClause::Single(FormatSpec::Json)));
    }

    // ── 9. EXISTS with hash ───────────────────────────────────────────────

    #[test]
    fn test_exists_hash() {
        let q = p("EXISTS sha256:abc123def456");
        match &q.statement {
            CalStatement::Exists(e) => {
                let wc = e.where_clause.as_ref().unwrap();
                match &wc.condition {
                    Condition::Comparison { field, value, .. } => {
                        assert_eq!(field, "hash");
                        assert_eq!(
                            *value,
                            Value::Hash {
                                value: "abc123def456".into()
                            }
                        );
                    }
                    other => panic!("expected Comparison, got {:?}", other),
                }
            }
            other => panic!("expected Exists, got {:?}", other),
        }
    }

    // ── 10. ASSEMBLE ──────────────────────────────────────────────────────

    #[test]
    fn test_assemble() {
        let q = p(r#"ASSEMBLE "daily_summary" FROM (RECALL facts RECENT 10)"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.topic, "daily_summary");
                match &a.from {
                    Source::Query(_) => {}
                    other => panic!("expected Query source, got {:?}", other),
                }
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── 11. EXPLAIN ───────────────────────────────────────────────────────

    #[test]
    fn test_explain() {
        let q = p(r#"EXPLAIN RECALL facts WHERE subject = "john""#);
        match &q.statement {
            CalStatement::Explain(e) => {
                matches!(*e.inner, CalStatement::Recall(_));
            }
            other => panic!("expected Explain, got {:?}", other),
        }
    }

    // ── 12. DESCRIBE ──────────────────────────────────────────────────────

    #[test]
    fn test_describe_grain_types() {
        let q = p("DESCRIBE grain_types");
        match &q.statement {
            CalStatement::Describe(_) => {}
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    // ── 13. BATCH ─────────────────────────────────────────────────────────

    #[test]
    fn test_batch() {
        let q = p("BATCH { recent: RECALL facts RECENT 5, ex: EXISTS sha256:abc123def456 }");
        match &q.statement {
            CalStatement::Batch(b) => {
                assert_eq!(b.statements.len(), 2);
                assert!(matches!(
                    &b.statements[0].statement,
                    CalStatement::Recall(_)
                ));
                assert!(matches!(
                    &b.statements[1].statement,
                    CalStatement::Exists(_)
                ));
            }
            other => panic!("expected Batch, got {:?}", other),
        }
    }

    // ── 14. COALESCE ──────────────────────────────────────────────────────

    #[test]
    fn test_coalesce() {
        let q = p(
            r#"COALESCE(RECALL facts WHERE subject = "john", RECALL facts WHERE subject = "bob")"#,
        );
        assert!(matches!(q.statement, CalStatement::Coalesce(_)));
    }

    // ── 15. Set operation (UNION) ─────────────────────────────────────────

    #[test]
    fn test_union() {
        let q = p(
            r#"(RECALL facts WHERE subject = "john") UNION (RECALL facts WHERE subject = "bob")"#,
        );
        match &q.statement {
            CalStatement::SetOp(s) => {
                assert_eq!(s.op, SetOp::Union);
                assert_eq!(s.operands.len(), 2);
            }
            other => panic!("expected SetOp, got {:?}", other),
        }
    }

    // ── 16. ADD (Tier 1) ──────────────────────────────────────────────────

    #[test]
    fn test_add_fact() {
        let q = p(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "coffee" REASON "user preference""#,
        );
        match &q.statement {
            CalStatement::Add(a) => {
                assert_eq!(a.grain_type, GrainTypeSingular::Fact);
                assert_eq!(a.fields.len(), 3);
                assert_eq!(a.fields[0].field, "subject");
            }
            other => panic!("expected Add, got {:?}", other),
        }
    }

    #[test]
    fn test_add_fact_reason_stored() {
        let q = p(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "coffee" REASON "user preference""#,
        );
        match &q.statement {
            CalStatement::Add(a) => {
                assert_eq!(a.reason, "user preference");
            }
            other => panic!("expected Add, got {:?}", other),
        }
    }

    #[test]
    fn test_add_fact_because_accepted() {
        let q = p(
            r#"ADD fact SET subject = "bob" SET relation = "likes" SET object = "rust" BECAUSE "test reason""#,
        );
        match &q.statement {
            CalStatement::Add(a) => {
                assert_eq!(a.grain_type, GrainTypeSingular::Fact);
                assert_eq!(a.reason, "test reason");
            }
            other => panic!("expected Add, got {:?}", other),
        }
    }

    #[test]
    fn test_add_fact_missing_reason_fails() {
        let input = r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "coffee""#;
        let err = parse(input).unwrap_err();
        assert!(
            matches!(err, CalError::MissingReason { .. }),
            "expected MissingReason, got: {:?}",
            err
        );
    }

    #[test]
    fn test_add_duplicate_set_field_warning() {
        let q = p(
            r#"ADD fact SET subject = "a" SET subject = "b" SET relation = "r" SET object = "o" REASON "test""#,
        );
        match &q.statement {
            CalStatement::Add(a) => {
                assert_eq!(a.fields.len(), 4);
                // The last value wins.
                assert_eq!(a.fields[0].field, "subject");
                assert_eq!(a.fields[1].field, "subject");
            }
            other => panic!("expected Add, got {:?}", other),
        }
        // Should have exactly one warning for duplicate "subject".
        assert_eq!(q.warnings.len(), 1);
        assert!(
            matches!(&q.warnings[0], CalWarning::DuplicateSetField { field, .. } if field == "subject"),
            "expected DuplicateSetField warning, got: {:?}",
            q.warnings
        );
    }

    // ── 16b. ADD WORKFLOW (graph syntax) ────────────────────────────────

    #[test]
    fn test_add_workflow_simple_linear() {
        let q = p(
            r#"ADD workflow "CI pipeline" build -> test -> deploy REASON "standard pipeline""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.name, "CI pipeline");
                assert_eq!(wf.nodes, vec!["build", "test", "deploy"]);
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].src, "build");
                assert_eq!(wf.edges[0].dst, "test");
                assert_eq!(wf.edges[1].src, "test");
                assert_eq!(wf.edges[1].dst, "deploy");
                assert_eq!(wf.reason, "standard pipeline");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn test_add_workflow_parallel() {
        let q = p(
            r#"ADD workflow "review" lint -> (security, compliance) -> evaluate REASON "parallel review""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["lint", "security", "compliance", "evaluate"]);
                // lint -> security, lint -> compliance
                // security -> evaluate, compliance -> evaluate
                assert_eq!(wf.edges.len(), 4);
                assert_eq!(wf.edges[0].src, "lint");
                assert_eq!(wf.edges[0].dst, "security");
                assert_eq!(wf.edges[1].src, "lint");
                assert_eq!(wf.edges[1].dst, "compliance");
                assert_eq!(wf.edges[2].src, "security");
                assert_eq!(wf.edges[2].dst, "evaluate");
                assert_eq!(wf.edges[3].src, "compliance");
                assert_eq!(wf.edges[3].dst, "evaluate");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn test_add_workflow_conditional() {
        let q = p(
            r#"ADD workflow "gate" evaluate -> implement WHEN "approved" evaluate -> reject WHEN "rejected" REASON "approval routing""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["evaluate", "implement", "reject"]);
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].cond, Some("approved".into()));
                assert_eq!(wf.edges[1].cond, Some("rejected".into()));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn test_add_workflow_retry() {
        let q = p(
            r#"ADD workflow "deploy" build -> deploy * 3 -> notify REASON "retry deploy""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["build", "deploy", "notify"]);
                assert_eq!(wf.edges.len(), 2);
                // build -> deploy (with repeat)
                assert_eq!(wf.edges[0].src, "build");
                assert_eq!(wf.edges[0].dst, "deploy");
                assert_eq!(wf.edges[0].repeat, Some(3));
                // deploy -> notify
                assert_eq!(wf.edges[1].src, "deploy");
                assert_eq!(wf.edges[1].dst, "notify");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn test_add_workflow_with_bindings() {
        let q = p(
            r#"ADD workflow "pipeline" build -> test BIND build = sha256:def11111 BIND test = sha256:def22222 REASON "bound pipeline""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.bindings.len(), 2);
                assert_eq!(wf.bindings[0].node, "build");
                assert_eq!(wf.bindings[0].hash, "def11111");
                assert_eq!(wf.bindings[1].node, "test");
                assert_eq!(wf.bindings[1].hash, "def22222");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn test_add_workflow_no_trigger() {
        let q = p(r#"ADD workflow "checklist" review -> test -> merge REASON "template""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.name, "checklist");
                assert_eq!(wf.nodes, vec!["review", "test", "merge"]);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn test_add_workflow_quoted_node_names() {
        let q = p(
            r#"ADD workflow "onboarding" "send welcome email" -> "schedule orientation" REASON "onboard""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["send welcome email", "schedule orientation"]);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn test_add_workflow_mixed_pipeline() {
        let q = p(
            r#"ADD workflow "release" build -> (unit_test, lint) -> integration_test integration_test -> stage_deploy * 3 stage_deploy -> approval approval -> prod_deploy WHEN "approved" approval -> rollback WHEN "rejected" REASON "release pipeline""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(
                    wf.nodes,
                    vec![
                        "build",
                        "unit_test",
                        "lint",
                        "integration_test",
                        "stage_deploy",
                        "approval",
                        "prod_deploy",
                        "rollback"
                    ]
                );
                // build->unit_test, build->lint, unit_test->integration_test, lint->integration_test,
                // integration_test->stage_deploy(*3), stage_deploy->approval,
                // approval->prod_deploy(WHEN approved), approval->rollback(WHEN rejected)
                assert_eq!(wf.edges.len(), 8);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    // ── 16c. ADD/SUPERSEDE WORKFLOW — exhaustive edge-case tests ────────

    // ---- Happy-path edge cases (should parse successfully) ----

    /// Single node, no edges — minimal valid workflow.
    #[test]
    fn test_add_workflow_single_node_no_edges() {
        let q = p(r#"ADD workflow "x" build REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.name, "x");
                assert_eq!(wf.nodes, vec!["build"]);
                assert!(wf.edges.is_empty());
                assert_eq!(wf.reason, "y");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Long chain: a -> b -> c -> d -> e -> f
    #[test]
    fn test_add_workflow_long_chain() {
        let q = p(r#"ADD workflow "long" a -> b -> c -> d -> e -> f REASON "chain""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c", "d", "e", "f"]);
                assert_eq!(wf.edges.len(), 5);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[1].src, "b");
                assert_eq!(wf.edges[1].dst, "c");
                assert_eq!(wf.edges[2].src, "c");
                assert_eq!(wf.edges[2].dst, "d");
                assert_eq!(wf.edges[3].src, "d");
                assert_eq!(wf.edges[3].dst, "e");
                assert_eq!(wf.edges[4].src, "e");
                assert_eq!(wf.edges[4].dst, "f");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Multiple separate chains (multiline in one query): a -> b then c -> d.
    /// These are disconnected subgraphs — parser should accept both chains.
    #[test]
    fn test_add_workflow_multiple_separate_chains() {
        let q = p(r#"ADD workflow "multi" a -> b c -> d REASON "two chains""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c", "d"]);
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[1].src, "c");
                assert_eq!(wf.edges[1].dst, "d");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Nested parallel: a -> (b, (c, d)) -> e — inner group (c, d) inside
    /// outer group. The parser should parse (c, d) as a nested group inside
    /// the outer group, but since parse_node_or_group calls parse_node_name
    /// for each element, and parse_node_name doesn't handle parens, this
    /// should actually fail. Let's verify the actual behavior.
    #[test]
    fn test_add_workflow_nested_parallel_fails() {
        // Nested groups like (b, (c, d)) are NOT supported because
        // parse_node_name doesn't handle LParen. The inner '(' would
        // be rejected as an unexpected token.
        let err = pe(r#"ADD workflow "x" a -> (b, (c, d)) -> e REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// WHEN + * N on same edge: a -> b WHEN "x" * 3 — precedence test.
    #[test]
    fn test_add_workflow_when_and_repeat_combined() {
        let q = p(r#"ADD workflow "x" a -> b WHEN "cond" * 3 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges.len(), 1);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[0].cond, Some("cond".into()));
                assert_eq!(wf.edges[0].repeat, Some(3));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Node names that look like identifiers with numbers: step1 -> step2.
    #[test]
    fn test_add_workflow_alphanumeric_node_names() {
        let q = p(r#"ADD workflow "x" step1 -> step2 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["step1", "step2"]);
                assert_eq!(wf.edges.len(), 1);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Underscore-heavy names: __init__ -> _cleanup_.
    #[test]
    fn test_add_workflow_underscore_names() {
        let q = p(r#"ADD workflow "x" __init__ -> _cleanup_ REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["__init__", "_cleanup_"]);
                assert_eq!(wf.edges.len(), 1);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Single-char names: a -> b -> c.
    #[test]
    fn test_add_workflow_single_char_names() {
        let q = p(r#"ADD workflow "x" a -> b -> c REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c"]);
                assert_eq!(wf.edges.len(), 2);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// BECAUSE as alternative to REASON.
    #[test]
    fn test_add_workflow_because_keyword() {
        let q = p(r#"ADD workflow "x" build -> test BECAUSE "alt keyword""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.reason, "alt keyword");
                assert_eq!(wf.nodes, vec!["build", "test"]);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Mixed quoted and bare node names: build -> "run tests" -> deploy.
    #[test]
    fn test_add_workflow_mixed_bare_and_quoted() {
        let q = p(r#"ADD workflow "x" build -> "run tests" -> deploy REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["build", "run tests", "deploy"]);
                assert_eq!(wf.edges.len(), 2);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Parallel with single element: a -> (b) -> c — degenerates to just b.
    #[test]
    fn test_add_workflow_single_element_parallel() {
        let q = p(r#"ADD workflow "x" a -> (b) -> c REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c"]);
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[1].src, "b");
                assert_eq!(wf.edges[1].dst, "c");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Same node referenced multiple times across chains (convergence): a -> c, b -> c.
    #[test]
    fn test_add_workflow_convergence_node() {
        let q = p(r#"ADD workflow "x" a -> c b -> c REASON "converge""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                // "c" should appear only once in nodes (deduplicated).
                assert_eq!(wf.nodes, vec!["a", "c", "b"]);
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "c");
                assert_eq!(wf.edges[1].src, "b");
                assert_eq!(wf.edges[1].dst, "c");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Empty bindings (no BIND clauses) — already tested implicitly,
    /// but verify the bindings field is empty.
    #[test]
    fn test_add_workflow_no_bind_clauses() {
        let q = p(r#"ADD workflow "x" a -> b REASON "no bindings""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert!(wf.bindings.is_empty());
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Many BIND clauses (10+).
    #[test]
    fn test_add_workflow_many_bind_clauses() {
        let q = p(concat!(
            r#"ADD workflow "x" a -> b -> c "#,
            "BIND a = sha256:aaa11111 ",
            "BIND b = sha256:bbb22222 ",
            "BIND c = sha256:ccc33333 ",
            "BIND a = sha256:aaa44444 ",
            "BIND b = sha256:bbb55555 ",
            "BIND c = sha256:ccc66666 ",
            "BIND a = sha256:aaa77777 ",
            "BIND b = sha256:bbb88888 ",
            "BIND c = sha256:ccc99999 ",
            "BIND a = sha256:aaa00000 ",
            r#"REASON "many bindings""#,
        ));
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.bindings.len(), 10);
                assert_eq!(wf.bindings[0].node, "a");
                assert_eq!(wf.bindings[0].hash, "aaa11111");
                assert_eq!(wf.bindings[9].node, "a");
                assert_eq!(wf.bindings[9].hash, "aaa00000");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// `ON "..."` was removed in 1.3, and is refused by name.
    ///
    /// It set a free-text field nothing ever read — not the scheduler, not the
    /// driver — so it described an activation condition that could not activate
    /// anything. Silently ignoring the clause would leave an author believing
    /// they had scheduled something, which is the failure with no symptom.
    #[test]
    fn add_workflow_refuses_the_removed_on_clause_and_says_where_triggers_live() {
        let e = parse(r#"ADD workflow "cron" ON "cron * * * * *" run_job REASON "scheduled""#)
            .expect_err("ON must be refused, not ignored");
        let msg = e.to_string();
        assert!(msg.contains("removed in 1.3"), "{msg}");
        assert!(msg.contains("trigger"), "the message must point at the replacement: {msg}");
    }

    /// Self-loop: a -> a — parser should accept (cycle detection is engine's job).
    #[test]
    fn test_add_workflow_self_loop() {
        let q = p(r#"ADD workflow "x" a -> a REASON "self loop""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a"]);
                assert_eq!(wf.edges.len(), 1);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "a");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Repeat with value 1 — edge case, allowed.
    #[test]
    fn test_add_workflow_repeat_one() {
        let q = p(r#"ADD workflow "x" a -> b * 1 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges[0].repeat, Some(1));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Large repeat count.
    #[test]
    fn test_add_workflow_repeat_large() {
        let q = p(r#"ADD workflow "x" a -> b * 100 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges[0].repeat, Some(100));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Multiple edges with different WHEN conditions.
    #[test]
    fn test_add_workflow_multiple_when_conditions() {
        let q = p(
            r#"ADD workflow "x" a -> b WHEN "yes" a -> c WHEN "no" a -> d WHEN "maybe" REASON "branching""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c", "d"]);
                assert_eq!(wf.edges.len(), 3);
                assert_eq!(wf.edges[0].cond, Some("yes".into()));
                assert_eq!(wf.edges[1].cond, Some("no".into()));
                assert_eq!(wf.edges[2].cond, Some("maybe".into()));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Parallel group at start of chain.
    #[test]
    fn test_add_workflow_parallel_at_start() {
        let q = p(r#"ADD workflow "x" (a, b) -> c REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c"]);
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "c");
                assert_eq!(wf.edges[1].src, "b");
                assert_eq!(wf.edges[1].dst, "c");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Parallel group at end of chain.
    #[test]
    fn test_add_workflow_parallel_at_end() {
        let q = p(r#"ADD workflow "x" a -> (b, c) REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c"]);
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[1].src, "a");
                assert_eq!(wf.edges[1].dst, "c");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Parallel-to-parallel: (a, b) -> (c, d) — full cross-product.
    #[test]
    fn test_add_workflow_parallel_to_parallel() {
        let q = p(r#"ADD workflow "x" (a, b) -> (c, d) REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c", "d"]);
                // Cross product: a->c, a->d, b->c, b->d
                assert_eq!(wf.edges.len(), 4);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "c");
                assert_eq!(wf.edges[1].src, "a");
                assert_eq!(wf.edges[1].dst, "d");
                assert_eq!(wf.edges[2].src, "b");
                assert_eq!(wf.edges[2].dst, "c");
                assert_eq!(wf.edges[3].src, "b");
                assert_eq!(wf.edges[3].dst, "d");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// WHEN condition with an empty string.
    #[test]
    fn test_add_workflow_when_empty_string() {
        let q = p(r#"ADD workflow "x" a -> b WHEN "" REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges[0].cond, Some("".into()));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Repeat on multiple edges in same chain.
    #[test]
    fn test_add_workflow_repeat_on_multiple_edges() {
        let q = p(r#"ADD workflow "x" a -> b * 2 -> c * 5 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].repeat, Some(2));
                assert_eq!(wf.edges[1].repeat, Some(5));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Large parallel group (5+ elements).
    #[test]
    fn test_add_workflow_large_parallel_group() {
        let q = p(r#"ADD workflow "x" init -> (a, b, c, d, e) -> done REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["init", "a", "b", "c", "d", "e", "done"]);
                // init->{a,b,c,d,e}: 5 edges + {a,b,c,d,e}->done: 5 edges = 10
                assert_eq!(wf.edges.len(), 10);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Diamond pattern: a -> (b, c) -> d (fork-join).
    #[test]
    fn test_add_workflow_diamond_pattern() {
        let q = p(r#"ADD workflow "diamond" a -> (b, c) -> d REASON "fork-join""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c", "d"]);
                assert_eq!(wf.edges.len(), 4);
                // a->b, a->c
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[1].src, "a");
                assert_eq!(wf.edges[1].dst, "c");
                // b->d, c->d
                assert_eq!(wf.edges[2].src, "b");
                assert_eq!(wf.edges[2].dst, "d");
                assert_eq!(wf.edges[3].src, "c");
                assert_eq!(wf.edges[3].dst, "d");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// WITH sync clause on workflow.
    #[test]
    fn test_add_workflow_with_sync() {
        let q = p(r#"ADD workflow "x" a -> b WITH sync REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.with_options.len(), 1);
                assert_eq!(wf.with_options[0], AddWithOption::Sync);
                assert_eq!(wf.reason, "y");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// WITH + BIND together on workflow.
    #[test]
    fn test_add_workflow_bind_and_with() {
        let q = p(concat!(
            r#"ADD workflow "x" a -> b "#,
            "BIND a = sha256:abc12345 ",
            "WITH sync ",
            r#"REASON "y""#,
        ));
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.bindings.len(), 1);
                assert_eq!(wf.with_options.len(), 1);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Node name that is same as workflow name (no collision).
    #[test]
    fn test_add_workflow_node_name_equals_workflow_name() {
        let q = p(r#"ADD workflow "build" build -> test REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.name, "build");
                assert_eq!(wf.nodes, vec!["build", "test"]);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Chain where WHEN applies only to one edge, not all.
    #[test]
    fn test_add_workflow_when_not_inherited() {
        let q = p(r#"ADD workflow "x" a -> b WHEN "ok" -> c REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges.len(), 2);
                // a -> b (WHEN "ok")
                assert_eq!(wf.edges[0].cond, Some("ok".into()));
                // b -> c (no condition)
                assert_eq!(wf.edges[1].cond, None);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Repeat (*) not inherited to next edge.
    #[test]
    fn test_add_workflow_repeat_not_inherited() {
        let q = p(r#"ADD workflow "x" a -> b * 3 -> c REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].repeat, Some(3));
                assert_eq!(wf.edges[1].repeat, None);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    // ---- Error cases (should fail with clear errors) ----

    /// Missing REASON clause.
    #[test]
    fn test_add_workflow_missing_reason() {
        let err = pe(r#"ADD workflow "x" a -> b"#);
        assert!(matches!(err, CalError::MissingReason { .. }));
    }

    /// Missing workflow name — string expected after "workflow".
    #[test]
    fn test_add_workflow_missing_name() {
        let err = pe(r#"ADD workflow ON "trigger" a -> b REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Empty graph (no nodes) — REASON immediately after name.
    #[test]
    fn test_add_workflow_empty_graph() {
        let err = pe(r#"ADD workflow "x" REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Reserved word as bare node — ON is a keyword, not an identifier.
    #[test]
    fn test_add_workflow_reserved_word_as_node() {
        // "ON" is consumed as the ON keyword by the lexer, so it's not
        // seen as an ident for graph_line_start; it becomes the ON trigger
        // parse path. "WHEN" after graph start is consumed as WHEN condition.
        // Using BIND as a bare node name would be consumed as BIND clause start.
        // These are all keywords and can't be used as bare node names.
        let err = pe(r#"ADD workflow "x" ON -> WHEN REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Repeat zero: * 0 should fail.
    #[test]
    fn test_add_workflow_repeat_zero() {
        let err = pe(r#"ADD workflow "x" a -> b * 0 REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Repeat negative: * -1 should fail. The lexer yields -1 as
    /// NumberLiteral(-1.0); (-1.0f64 as u32) saturates to 0 in Rust,
    /// so it triggers the n == 0 check.
    #[test]
    fn test_add_workflow_repeat_negative() {
        let err = pe(r#"ADD workflow "x" a -> b * -1 REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Dangling arrow: a -> (nothing after).
    #[test]
    fn test_add_workflow_dangling_arrow() {
        let err = pe(r#"ADD workflow "x" a -> REASON "y""#);
        // After `->`, parser expects a node name or group. REASON is a keyword,
        // not an identifier, so parse_node_name fails with UnexpectedToken.
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Double arrow: a -> -> b.
    #[test]
    fn test_add_workflow_double_arrow() {
        let err = pe(r#"ADD workflow "x" a -> -> b REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Empty parallel group: a -> () -> b.
    #[test]
    fn test_add_workflow_empty_parallel_group() {
        let err = pe(r#"ADD workflow "x" a -> () -> b REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Unclosed paren: a -> (b, c REASON "y".
    #[test]
    fn test_add_workflow_unclosed_paren() {
        let err = pe(r#"ADD workflow "x" a -> (b, c REASON "y""#);
        // Parser expects RParen but finds REASON.
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// BIND with non-hash: BIND build = "not_a_hash".
    #[test]
    fn test_add_workflow_bind_non_hash() {
        let err = pe(r#"ADD workflow "x" a -> b BIND a = "not_a_hash" REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// WHEN without string: a -> b WHEN 42.
    #[test]
    fn test_add_workflow_when_without_string() {
        let err = pe(r#"ADD workflow "x" a -> b WHEN 42 REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Arrow at start with no source: -> a -> b.
    /// The `->` is an Arrow token; is_graph_line_start doesn't match Arrow,
    /// so parse_workflow_graph sees nothing and errors with "at least one node".
    #[test]
    fn test_add_workflow_arrow_at_start() {
        let err = pe(r#"ADD workflow "x" -> a -> b REASON "y""#);
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    /// Trailing comma in parallel group: (a, b,).
    #[test]
    fn test_add_workflow_trailing_comma_in_group() {
        let err = pe(r#"ADD workflow "x" (a, b,) -> c REASON "y""#);
        // After the trailing comma, parser tries parse_node_name and gets ')'.
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
    }

    #[test]
    fn test_workflow_error_messages_quality() {
        // This test verifies that error messages are specific and helpful.
        let cases: Vec<(&str, &str, &str)> = vec![
            (r#"ADD workflow "x" a -> b"#, "missing_reason", "REASON"),
            (
                r#"ADD workflow ON "t" a -> b REASON "y""#,
                "missing_name",
                "string",
            ),
            (r#"ADD workflow "x" REASON "y""#, "empty_graph", "node"),
            (
                r#"ADD workflow "x" a -> b * 0 REASON "y""#,
                "repeat_zero",
                ">= 1",
            ),
            (
                r#"ADD workflow "x" a -> REASON "y""#,
                "dangling_arrow",
                "node name",
            ),
            (
                r#"ADD workflow "x" a -> -> b REASON "y""#,
                "double_arrow",
                "node name",
            ),
            (
                r#"ADD workflow "x" a -> () -> b REASON "y""#,
                "empty_parens",
                "node name",
            ),
            (
                r#"ADD workflow "x" a -> (b, c REASON "y""#,
                "unclosed_paren",
                ")",
            ),
            (
                r#"ADD workflow "x" a -> b WHEN 42 REASON "y""#,
                "when_no_string",
                "string",
            ),
        ];
        for (input, label, expected_substring) in &cases {
            let err = pe(input);
            let msg = format!("{}", err);
            assert!(
                msg.to_lowercase()
                    .contains(&expected_substring.to_lowercase()),
                "Error for '{}' should mention '{}', got: {}",
                label,
                expected_substring,
                msg
            );
        }
    }

    /// Using a reserved word in a quoted node name (should work).
    #[test]
    fn test_add_workflow_reserved_word_quoted() {
        let q = p(r#"ADD workflow "x" "ON" -> "WHEN" -> "BIND" REASON "quoted keywords""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["ON", "WHEN", "BIND"]);
                assert_eq!(wf.edges.len(), 2);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Repeat with fractional number: * 2.5 — parser casts to u32.
    #[test]
    fn test_add_workflow_repeat_fractional() {
        let q = p(r#"ADD workflow "x" a -> b * 2.5 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                // 2.5f64 as u32 = 2 (truncation).
                assert_eq!(wf.edges[0].repeat, Some(2));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Duplicate node names across edges — parser deduplicates in nodes list.
    #[test]
    fn test_add_workflow_deduplicates_nodes() {
        let q = p(r#"ADD workflow "x" a -> b -> a -> c REASON "cycle""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                // "a" appears twice in edges, but should appear once in nodes.
                assert_eq!(wf.nodes, vec!["a", "b", "c"]);
                assert_eq!(wf.edges.len(), 3);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[1].src, "b");
                assert_eq!(wf.edges[1].dst, "a");
                assert_eq!(wf.edges[2].src, "a");
                assert_eq!(wf.edges[2].dst, "c");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Disconnected subgraphs: a -> b and c -> d (no link between them).
    #[test]
    fn test_add_workflow_disconnected_subgraphs() {
        let q = p(r#"ADD workflow "x" a -> b c -> d REASON "disconnected""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c", "d"]);
                assert_eq!(wf.edges.len(), 2);
                // Two independent edges, no connection between them.
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[1].src, "c");
                assert_eq!(wf.edges[1].dst, "d");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Single node in a separate chain (no edges) — just isolated nodes.
    #[test]
    fn test_add_workflow_isolated_nodes_multiple_chains() {
        let q = p(r#"ADD workflow "x" a b c REASON "isolated""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c"]);
                assert!(wf.edges.is_empty());
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    // ---- SUPERSEDE workflow tests ----

    /// Basic SUPERSEDE workflow.
    #[test]
    fn test_supersede_workflow_basic() {
        let q = p(r#"SUPERSEDE sha256:abc12345 a -> b REASON "updated""#);
        match &q.statement {
            CalStatement::SupersedeWorkflow(wf) => {
                assert_eq!(wf.hash, "abc12345");
                assert_eq!(wf.nodes, vec!["a", "b"]);
                assert_eq!(wf.edges.len(), 1);
                assert_eq!(wf.reason, "updated");
            }
            other => panic!("expected SupersedeWorkflow, got {:?}", other),
        }
    }

    /// SUPERSEDE workflow without ON trigger.
    #[test]
    fn test_supersede_workflow_no_trigger() {
        let q = p(r#"SUPERSEDE sha256:def45678 a -> b REASON "y""#);
        match &q.statement {
            CalStatement::SupersedeWorkflow(wf) => {
                assert_eq!(wf.hash, "def45678");
                assert_eq!(wf.nodes, vec!["a", "b"]);
            }
            other => panic!("expected SupersedeWorkflow, got {:?}", other),
        }
    }

    /// SUPERSEDE workflow with BIND clauses.
    #[test]
    fn test_supersede_workflow_with_bindings() {
        let q = p(concat!(
            "SUPERSEDE sha256:abc12345 a -> b -> c ",
            "BIND a = sha256:def11111 ",
            "BIND b = sha256:def22222 ",
            r#"REASON "bound""#,
        ));
        match &q.statement {
            CalStatement::SupersedeWorkflow(wf) => {
                assert_eq!(wf.bindings.len(), 2);
                assert_eq!(wf.bindings[0].node, "a");
                assert_eq!(wf.bindings[0].hash, "def11111");
                assert_eq!(wf.bindings[1].node, "b");
                assert_eq!(wf.bindings[1].hash, "def22222");
            }
            other => panic!("expected SupersedeWorkflow, got {:?}", other),
        }
    }

    /// SUPERSEDE workflow with parallel + WHEN + repeat.
    #[test]
    fn test_supersede_workflow_complex_graph() {
        let q = p(concat!(
            r#"SUPERSEDE sha256:aaaa1111 "#,
            r#"build -> (test, lint) -> deploy WHEN "pass" * 3 "#,
            r#"REASON "complex supersede""#,
        ));
        match &q.statement {
            CalStatement::SupersedeWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["build", "test", "lint", "deploy"]);
                // build->test, build->lint, test->deploy(WHEN+*3), lint->deploy(WHEN+*3)
                assert_eq!(wf.edges.len(), 4);
                assert_eq!(wf.edges[2].cond, Some("pass".into()));
                assert_eq!(wf.edges[2].repeat, Some(3));
                assert_eq!(wf.edges[3].cond, Some("pass".into()));
                assert_eq!(wf.edges[3].repeat, Some(3));
            }
            other => panic!("expected SupersedeWorkflow, got {:?}", other),
        }
    }

    /// SUPERSEDE workflow missing REASON.
    #[test]
    fn test_supersede_workflow_missing_reason() {
        let err = pe("SUPERSEDE sha256:abc12345 a -> b");
        assert!(matches!(err, CalError::MissingReason { .. }));
    }

    // ---- Integration with existing CAL features ----

    /// ADD workflow inside BATCH.
    #[test]
    fn test_add_workflow_in_batch() {
        let q = p("BATCH { ADD workflow \"x\" a -> b REASON \"y\" }");
        match &q.statement {
            CalStatement::Batch(b) => {
                assert_eq!(b.statements.len(), 1);
                assert!(matches!(
                    &b.statements[0].statement,
                    CalStatement::AddWorkflow(_)
                ));
            }
            other => panic!("expected Batch, got {:?}", other),
        }
    }

    /// EXPLAIN ADD workflow.
    #[test]
    fn test_explain_add_workflow() {
        let q = p(r#"EXPLAIN ADD workflow "x" a -> b REASON "y""#);
        match &q.statement {
            CalStatement::Explain(e) => {
                assert!(matches!(*e.inner, CalStatement::AddWorkflow(_)));
            }
            other => panic!("expected Explain, got {:?}", other),
        }
    }

    /// Multiple workflows in BATCH.
    #[test]
    fn test_multiple_workflows_in_batch() {
        let q = p(concat!(
            "BATCH { ",
            r#"ADD workflow "first" a -> b REASON "r1" ; "#,
            r#"ADD workflow "second" c -> d REASON "r2" ; "#,
            "}",
        ));
        match &q.statement {
            CalStatement::Batch(b) => {
                assert_eq!(b.statements.len(), 2);
                match &b.statements[0].statement {
                    CalStatement::AddWorkflow(wf) => assert_eq!(wf.name, "first"),
                    other => panic!("expected AddWorkflow, got {:?}", other),
                }
                match &b.statements[1].statement {
                    CalStatement::AddWorkflow(wf) => assert_eq!(wf.name, "second"),
                    other => panic!("expected AddWorkflow, got {:?}", other),
                }
            }
            other => panic!("expected Batch, got {:?}", other),
        }
    }

    /// SUPERSEDE workflow inside BATCH.
    #[test]
    fn test_supersede_workflow_in_batch() {
        let q = p(concat!(
            "BATCH { ",
            r#"SUPERSEDE sha256:abc12345 a -> b REASON "y" ; "#,
            "}",
        ));
        match &q.statement {
            CalStatement::Batch(b) => {
                assert_eq!(b.statements.len(), 1);
                assert!(matches!(
                    &b.statements[0].statement,
                    CalStatement::SupersedeWorkflow(_)
                ));
            }
            other => panic!("expected Batch, got {:?}", other),
        }
    }

    /// EXPLAIN SUPERSEDE workflow.
    #[test]
    fn test_explain_supersede_workflow() {
        let q = p(r#"EXPLAIN SUPERSEDE sha256:abc12345 a -> b REASON "y""#);
        match &q.statement {
            CalStatement::Explain(e) => {
                assert!(matches!(*e.inner, CalStatement::SupersedeWorkflow(_)));
            }
            other => panic!("expected Explain, got {:?}", other),
        }
    }

    /// Workflow with only quoted node names.
    #[test]
    fn test_add_workflow_all_quoted_names() {
        let q =
            p(r#"ADD workflow "proc" "step one" -> "step two" -> "step three" REASON "quoted""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["step one", "step two", "step three"]);
                assert_eq!(wf.edges.len(), 2);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Parallel group with all quoted names.
    #[test]
    fn test_add_workflow_parallel_all_quoted() {
        let q = p(
            r#"ADD workflow "x" start -> ("unit tests", "integration tests") -> deploy REASON "y""#,
        );
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(
                    wf.nodes,
                    vec!["start", "unit tests", "integration tests", "deploy"]
                );
                assert_eq!(wf.edges.len(), 4);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Three parallel groups in sequence: (a,b) -> (c,d) -> (e,f).
    #[test]
    fn test_add_workflow_three_parallel_groups() {
        let q = p(r#"ADD workflow "x" (a, b) -> (c, d) -> (e, f) REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c", "d", "e", "f"]);
                // (a,b)->(c,d): 4 edges; (c,d)->(e,f): 4 edges = 8 total
                assert_eq!(wf.edges.len(), 8);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// WHEN on edge from parallel group: (a,b) -> c WHEN "ok".
    /// Both a->c and b->c get the WHEN condition.
    #[test]
    fn test_add_workflow_when_on_parallel_to_single() {
        let q = p(r#"ADD workflow "x" (a, b) -> c WHEN "ok" REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].cond, Some("ok".into()));
                assert_eq!(wf.edges[1].cond, Some("ok".into()));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Repeat on edge to parallel group: a -> (b,c) * 3.
    /// Both a->b and a->c get the repeat count.
    #[test]
    fn test_add_workflow_repeat_on_single_to_parallel() {
        let q = p(r#"ADD workflow "x" a -> (b, c) * 3 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].repeat, Some(3));
                assert_eq!(wf.edges[1].repeat, Some(3));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// BIND to a node that doesn't exist in the graph — parser accepts
    /// (semantic validation is the engine's job).
    #[test]
    fn test_add_workflow_bind_nonexistent_node() {
        let q = p(r#"ADD workflow "x" a -> b BIND nonexistent = sha256:abc12345 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.bindings.len(), 1);
                assert_eq!(wf.bindings[0].node, "nonexistent");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// A `SUPERSEDE` of a plan also refuses the removed `ON` clause, rather
    /// than treating it as the start of a graph line and failing obscurely.
    #[test]
    fn supersede_workflow_refuses_the_removed_on_clause() {
        let e = parse(
            r#"SUPERSEDE sha256:1111111111111111111111111111111111111111111111111111111111111111 ON "x" a -> b REASON "y""#,
        )
        .expect_err("ON must not be accepted on a supersede either");
        assert!(!e.to_string().is_empty());
    }

    /// Workflow name with unicode.
    #[test]
    fn test_add_workflow_unicode_name() {
        let q = p("ADD workflow \"\u{1F680} deploy\" a -> b REASON \"y\"");
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.name, "\u{1F680} deploy");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// SUPERSEDE workflow — single node, no edges.
    #[test]
    fn test_supersede_workflow_single_node() {
        let q = p(r#"SUPERSEDE sha256:abc12345 run REASON "minimal""#);
        match &q.statement {
            CalStatement::SupersedeWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["run"]);
                assert!(wf.edges.is_empty());
            }
            other => panic!("expected SupersedeWorkflow, got {:?}", other),
        }
    }

    /// SUPERSEDE workflow — BECAUSE instead of REASON.
    #[test]
    fn test_supersede_workflow_because() {
        let q = p(r#"SUPERSEDE sha256:abc12345 a -> b BECAUSE "alt""#);
        match &q.statement {
            CalStatement::SupersedeWorkflow(wf) => {
                assert_eq!(wf.reason, "alt");
            }
            other => panic!("expected SupersedeWorkflow, got {:?}", other),
        }
    }

    /// Verify edge order in complex workflow: edges should follow
    /// chain-by-chain, segment-by-segment order.
    #[test]
    fn test_add_workflow_edge_order() {
        let q = p(concat!(
            r#"ADD workflow "x" "#,
            "a -> b -> c ",
            "d -> e ",
            r#"REASON "y""#,
        ));
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.edges.len(), 3);
                // Chain 1: a->b, b->c
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[1].src, "b");
                assert_eq!(wf.edges[1].dst, "c");
                // Chain 2: d->e
                assert_eq!(wf.edges[2].src, "d");
                assert_eq!(wf.edges[2].dst, "e");
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Node names with many underscores and numbers.
    #[test]
    fn test_add_workflow_complex_ident_names() {
        let q = p(r#"ADD workflow "x" a1_b2_c3 -> x99_y00 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a1_b2_c3", "x99_y00"]);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Only a parallel group, no edges.
    #[test]
    fn test_add_workflow_parallel_group_only() {
        let q = p(r#"ADD workflow "x" (a, b, c) REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                assert_eq!(wf.nodes, vec!["a", "b", "c"]);
                assert!(wf.edges.is_empty());
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    // ── Workflow hardening tests (audit issues) ────────────────────────────

    /// Issue 2: WHEN on a bare node (not after an edge) should give a
    /// specific error telling the user WHEN must follow `->`.
    #[test]
    fn test_add_workflow_when_on_non_edge_context() {
        let err = pe(r#"ADD workflow "x" a WHEN "cond" -> b REASON "y""#);
        let msg = format!("{}", err);
        assert!(
            msg.to_lowercase().contains("when") && msg.to_lowercase().contains("edge"),
            "expected error about WHEN placement, got: {}",
            msg
        );
    }

    /// Issue 3a: Dangling arrow — `a ->` followed by REASON.
    #[test]
    fn test_add_workflow_dangling_arrow_message() {
        let err = pe(r#"ADD workflow "x" a -> REASON "y""#);
        let msg = format!("{}", err);
        assert!(
            msg.to_lowercase().contains("node name"),
            "expected error mentioning 'node name', got: {}",
            msg
        );
    }

    /// Issue 3b: Double arrow — `a -> -> b`.
    #[test]
    fn test_add_workflow_double_arrow_message() {
        let err = pe(r#"ADD workflow "x" a -> -> b REASON "y""#);
        let msg = format!("{}", err);
        assert!(
            msg.to_lowercase().contains("node name") && msg.to_lowercase().contains("->"),
            "expected error about node name after ->, got: {}",
            msg
        );
    }

    /// Issue 3c: Empty group — `a -> () -> b`.
    #[test]
    fn test_add_workflow_empty_group_message() {
        let err = pe(r#"ADD workflow "x" a -> () -> b REASON "y""#);
        let msg = format!("{}", err);
        assert!(
            msg.to_lowercase().contains("node name"),
            "expected error about empty group needing a node, got: {}",
            msg
        );
    }

    /// Issue 3d: Unclosed paren — `a -> (b, c REASON "y"`.
    #[test]
    fn test_add_workflow_unclosed_paren_message() {
        let err = pe(r#"ADD workflow "x" a -> (b, c REASON "y""#);
        let msg = format!("{}", err);
        assert!(
            msg.contains(")") || msg.to_lowercase().contains("close"),
            "expected error mentioning ')' to close group, got: {}",
            msg
        );
    }

    /// Issue 3e: No graph body — just REASON after name.
    #[test]
    fn test_add_workflow_no_graph_message() {
        let err = pe(r#"ADD workflow "x" REASON "y""#);
        let msg = format!("{}", err);
        assert!(
            msg.to_lowercase().contains("node"),
            "expected error mentioning nodes needed, got: {}",
            msg
        );
    }

    /// Issue 3f: Zero repeat — `* 0`.
    #[test]
    fn test_add_workflow_repeat_zero_message() {
        let err = pe(r#"ADD workflow "x" a -> b * 0 REASON "y""#);
        let msg = format!("{}", err);
        assert!(
            msg.contains(">= 1") || msg.to_lowercase().contains("at least"),
            "expected error about repeat count >= 1, got: {}",
            msg
        );
    }

    /// Issue 3g: Non-number after `*` — `a -> b * abc`.
    #[test]
    fn test_add_workflow_repeat_non_number() {
        let err = pe(r#"ADD workflow "x" a -> b * abc REASON "y""#);
        let msg = format!("{}", err);
        assert!(
            msg.to_lowercase().contains("number"),
            "expected error about expecting number after *, got: {}",
            msg
        );
    }

    /// Issue 4: ON/WHEN/BIND as field names in non-workflow contexts
    /// should still work — they must not be broken by the keyword tokens.
    #[test]
    fn test_on_as_field_name_in_where() {
        // `on` used as a field name in a RECALL WHERE clause.
        let q = p(r#"RECALL facts WHERE on = "something""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                assert!(r.where_clause.is_some());
            }
            other => panic!("expected Recall, got {:?}", other),
        }
    }

    #[test]
    fn test_when_as_field_name_in_where() {
        let q = p(r#"RECALL events WHERE when = "2025-01-01""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                assert!(r.where_clause.is_some());
            }
            other => panic!("expected Recall, got {:?}", other),
        }
    }

    #[test]
    fn test_bind_as_field_name_in_where() {
        let q = p(r#"RECALL facts WHERE bind = "test""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                assert!(r.where_clause.is_some());
            }
            other => panic!("expected Recall, got {:?}", other),
        }
    }

    #[test]
    fn test_on_as_field_name_in_add() {
        // `on` used as a field name in an ADD SET clause.
        let q = p(r#"ADD fact SET on = "value" REASON "y""#);
        match &q.statement {
            CalStatement::Add(a) => {
                assert!(a.fields.iter().any(|f| f.field == "on"));
            }
            other => panic!("expected Add, got {:?}", other),
        }
    }

    /// Issue 5: `* N` attaches to dst node in retries map.
    /// In `a -> b * 3 -> c`, the retries map gets `"b": 3` and
    /// edges are `a->b`, `b->c` (both without max_cycles on them).
    #[test]
    fn test_add_workflow_repeat_attaches_to_target() {
        let q = p(r#"ADD workflow "x" a -> b * 3 -> c REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                // 2 edges: a->b, b->c
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].src, "a");
                assert_eq!(wf.edges[0].dst, "b");
                assert_eq!(wf.edges[0].repeat, Some(3));
                assert_eq!(wf.edges[1].src, "b");
                assert_eq!(wf.edges[1].dst, "c");
                assert_eq!(wf.edges[1].repeat, None);
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// Issue 6: SUPERSEDE with SET (non-workflow) correctly routes to
    /// the SET path even after workflow detection is added.
    #[test]
    fn test_supersede_set_not_confused_with_workflow() {
        let q = p(r#"SUPERSEDE sha256:abc12345 SET object = "new" REASON "updated""#);
        match &q.statement {
            CalStatement::Supersede(s) => {
                assert_eq!(s.hash, "abc12345");
                assert_eq!(s.set_clauses.len(), 1);
                assert_eq!(s.set_clauses[0].field, "object");
                assert_eq!(s.reason, "updated");
            }
            other => panic!(
                "expected Supersede (not SupersedeWorkflow), got {:?}",
                other
            ),
        }
    }

    /// Issue 5 edge case: repeat on parallel targets — each target gets
    /// its own retry count entry.
    #[test]
    fn test_add_workflow_repeat_on_parallel_targets() {
        let q = p(r#"ADD workflow "x" a -> (b, c) * 2 REASON "y""#);
        match &q.statement {
            CalStatement::AddWorkflow(wf) => {
                // 2 edges: a->b, a->c — both with repeat=2
                assert_eq!(wf.edges.len(), 2);
                assert_eq!(wf.edges[0].repeat, Some(2));
                assert_eq!(wf.edges[1].repeat, Some(2));
            }
            other => panic!("expected AddWorkflow, got {:?}", other),
        }
    }

    /// ON/WHEN/BIND used as SELECT field names in pipelines.
    #[test]
    fn test_keyword_as_select_field() {
        let q = p(r#"RECALL facts WHERE subject = "x" | SELECT on, when, bind"#);
        match &q.pipeline.first() {
            Some(PipelineStage::Select { fields, .. }) => {
                assert_eq!(fields, &["on", "when", "bind"]);
            }
            other => panic!("expected Select pipeline stage, got {:?}", other),
        }
    }

    #[test]
    fn test_priority_as_field_name_in_where() {
        let q = p(r#"RECALL goals WHERE priority = "high""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                assert!(r.where_clause.is_some());
            }
            other => panic!("expected Recall, got {:?}", other),
        }
    }

    #[test]
    fn test_scope_as_field_name_in_where() {
        let q = p(r#"RECALL consents WHERE scope = "read""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                assert!(r.where_clause.is_some());
            }
            other => panic!("expected Recall, got {:?}", other),
        }
    }

    #[test]
    fn test_priority_scope_as_select_fields() {
        let q = p(r#"RECALL goals WHERE subject = "x" | SELECT priority, scope"#);
        match &q.pipeline.first() {
            Some(PipelineStage::Select { fields, .. }) => {
                assert!(fields.contains(&"priority".to_string()));
                assert!(fields.contains(&"scope".to_string()));
            }
            other => panic!("expected Select pipeline stage, got {:?}", other),
        }
    }

    #[test]
    fn test_multiple_with_clauses_merged() {
        let q = p("RECALL facts WITH superseded WITH score_breakdown WITH explanation");
        assert!(q.with_options.contains(&WithOption::Superseded));
        assert!(q.with_options.contains(&WithOption::ScoreBreakdown));
        assert!(q.with_options.contains(&WithOption::Explanation));
        assert_eq!(q.with_options.len(), 3);
    }

    #[test]
    fn test_multiple_with_clauses_mixed_with_comma() {
        let q = p("RECALL facts WITH superseded, score_breakdown WITH explanation");
        assert!(q.with_options.contains(&WithOption::Superseded));
        assert!(q.with_options.contains(&WithOption::ScoreBreakdown));
        assert!(q.with_options.contains(&WithOption::Explanation));
        assert_eq!(q.with_options.len(), 3);
    }

    // ── 17. SUPERSEDE (Tier 1) ────────────────────────────────────────────

    #[test]
    fn test_supersede() {
        let q = p(
            r#"SUPERSEDE sha256:abc123def456 SET object = "light mode" REASON "changed preference""#,
        );
        match &q.statement {
            CalStatement::Supersede(s) => {
                assert_eq!(s.hash, "abc123def456");
                assert_eq!(s.reason, "changed preference");
                assert_eq!(s.set_clauses.len(), 1);
            }
            other => panic!("expected Supersede, got {:?}", other),
        }
    }

    // ── 18. REVERT (Tier 1) ───────────────────────────────────────────────

    #[test]
    fn test_revert() {
        let q = p(r#"REVERT sha256:abc123def456 REASON "mistake""#);
        match &q.statement {
            CalStatement::Revert(r) => {
                assert_eq!(r.hash, "abc123def456");
                assert_eq!(r.reason, "mistake");
            }
            other => panic!("expected Revert, got {:?}", other),
        }
    }

    // ── 19. LET binding ───────────────────────────────────────────────────

    #[test]
    fn test_let_binding() {
        let q = p("LET $users = RECALL facts SUBJECTS; RECALL events");
        assert_eq!(q.let_bindings.len(), 1);
        assert_eq!(q.let_bindings[0].name, "users");
        assert_eq!(q.let_bindings[0].extractor, Extractor::Subjects);
    }

    #[test]
    fn test_let_binding_pipe_backward_compat() {
        let q = p("LET $users = RECALL facts | SUBJECTS; RECALL events");
        assert_eq!(q.let_bindings.len(), 1);
        assert_eq!(q.let_bindings[0].name, "users");
        assert_eq!(q.let_bindings[0].extractor, Extractor::Subjects);
    }

    // ── 20. WHERE with AND/OR ─────────────────────────────────────────────

    #[test]
    fn test_where_and() {
        let q = p(r#"RECALL facts WHERE subject = "john" AND confidence >= 0.8"#);
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                assert!(matches!(cond, Condition::And { .. }));
            }
            _ => panic!("expected Recall"),
        }
    }

    #[test]
    fn test_where_or() {
        let q = p(r#"RECALL facts WHERE subject = "john" OR subject = "bob""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                assert!(matches!(cond, Condition::Or { .. }));
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 21. Complex nested query ──────────────────────────────────────────

    #[test]
    fn test_complex_nested() {
        let q = p(
            r#"CAL/1 RECALL facts ABOUT "preferences" WHERE confidence >= 0.8 ORDER BY confidence DESC LIMIT 5 WITH score_breakdown FORMAT json"#,
        );
        assert_eq!(q.version, CalVersion(1));
        assert!(matches!(q.statement, CalStatement::Recall(_)));
        assert_eq!(q.pipeline.len(), 2);
        assert!(q.with_options.contains(&WithOption::ScoreBreakdown));
        assert_eq!(q.format, Some(FormatClause::Single(FormatSpec::Json)));
    }

    // ── 22. Error: destructive keyword ────────────────────────────────────

    #[test]
    fn test_error_destructive_keyword() {
        // DELETE and FORGET are now valid CAL statements; use ERASE instead.
        let err = pe("ERASE facts WHERE subject = \"john\"");
        assert!(matches!(err, CalError::UnexpectedToken { .. }));
        assert!(err.suggestion().is_some());
        let sug = err.suggestion().unwrap();
        assert!(sug.contains("CAL cannot destroy data in bulk"));
        // FORGET <hash> is real grammar and may be advertised; blocked
        // keywords and PURGE (token, but not text grammar) must not be.
        let msg = err.to_string();
        assert!(!msg.contains("PURGE"), "{msg}");
        assert!(!msg.contains("DELETE"), "{msg}");
    }

    // ── 23. Error: unknown grain type ─────────────────────────────────────

    #[test]
    fn test_error_unknown_grain_type_beliefs() {
        let err = pe("RECALL beliefs");
        match err {
            CalError::UnknownGrainType {
                found, suggestion, ..
            } => {
                assert_eq!(found, "beliefs");
                assert!(suggestion.unwrap().contains("facts"));
            }
            other => panic!("expected UnknownGrainType, got {:?}", other),
        }
    }

    // ── 24. Version prefix ────────────────────────────────────────────────

    #[test]
    fn test_version_prefix() {
        let q = p("CAL/1 RECALL facts");
        assert_eq!(q.version, CalVersion(1));
    }

    #[test]
    fn test_unsupported_version() {
        let err = pe("CAL/2 RECALL facts");
        assert!(matches!(
            err,
            CalError::UnsupportedVersion { version: 2, .. }
        ));
    }

    // ── 25. HISTORY ───────────────────────────────────────────────────────

    #[test]
    fn test_history() {
        let q = p("HISTORY sha256:abc123def456");
        match &q.statement {
            CalStatement::History(h) => {
                assert_eq!(h.hash, "abc123def456");
            }
            other => panic!("expected History, got {:?}", other),
        }
    }

    // ── 26. WHERE with IN ─────────────────────────────────────────────────

    #[test]
    fn test_where_in_list() {
        let q = p(r#"RECALL facts WHERE subject IN ("john", "bob")"#);
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                match cond {
                    Condition::In { field, values, .. } => {
                        assert_eq!(field, "subject");
                        assert_eq!(values.len(), 2);
                    }
                    other => panic!("expected In, got {:?}", other),
                }
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 27. WHERE with multiple AND conditions ────────────────────────────

    #[test]
    fn test_where_multiple_and() {
        let q = p(
            r#"RECALL facts WHERE subject = "john" AND confidence >= 0.8 AND tags INCLUDE ["preferences"]"#,
        );
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                // Should be And(And(comparison, comparison), comparison)
                assert!(matches!(cond, Condition::And { .. }));
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 28. Error: empty query ────────────────────────────────────────────

    #[test]
    fn test_error_empty_query() {
        let err = pe("");
        assert!(matches!(err, CalError::EmptyQuery { .. }));
    }

    // ── 29. Error: query too long ─────────────────────────────────────────

    #[test]
    fn test_error_query_too_long() {
        // String must exceed MAX_QUERY_LENGTH (65536) to trigger the pre-parse length check.
        let huge = "RECALL facts WHERE subject = \"".to_string() + &"a".repeat(66_000) + "\"";
        let err = pe(&huge);
        assert!(matches!(err, CalError::QueryTooLong { .. }));
    }

    // ── 30. WHERE IS NULL / IS NOT NULL ───────────────────────────────────

    #[test]
    fn test_where_is_null() {
        let q = p("RECALL facts WHERE object IS NULL");
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                assert!(matches!(cond, Condition::IsNull { field, .. } if field == "object"));
            }
            _ => panic!("expected Recall"),
        }
    }

    #[test]
    fn test_where_is_not_null() {
        let q = p("RECALL facts WHERE object IS NOT NULL");
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                assert!(matches!(cond, Condition::IsNotNull { field, .. } if field == "object"));
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 31. Format TEMPLATE ───────────────────────────────────────────────

    #[test]
    fn test_format_template() {
        let q = p(r#"RECALL facts FORMAT TEMPLATE "{{subject}}: {{object}}""#);
        match &q.format {
            Some(FormatClause::Single(FormatSpec::Template { template })) => {
                assert!(template.contains("subject"));
            }
            other => panic!("expected Template format, got {:?}", other),
        }
    }

    // ── 32. Intersect set operation ───────────────────────────────────────

    #[test]
    fn test_intersect() {
        let q = p(
            r#"(RECALL facts WHERE subject = "john") INTERSECT (RECALL facts WHERE confidence >= 0.9)"#,
        );
        match &q.statement {
            CalStatement::SetOp(s) => {
                assert_eq!(s.op, SetOp::Intersect);
            }
            other => panic!("expected SetOp, got {:?}", other),
        }
    }

    // ── 33. EXCEPT set operation ──────────────────────────────────────────

    #[test]
    fn test_except() {
        let q = p(
            r#"(RECALL facts WHERE subject = "john") EXCEPT (RECALL facts WHERE confidence < 0.5)"#,
        );
        match &q.statement {
            CalStatement::SetOp(s) => {
                assert_eq!(s.op, SetOp::Except);
            }
            other => panic!("expected SetOp, got {:?}", other),
        }
    }

    // ── 34. NOT condition ─────────────────────────────────────────────────

    #[test]
    fn test_where_not() {
        let q = p(r#"RECALL facts WHERE NOT subject = "john""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                assert!(matches!(cond, Condition::Not { .. }));
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 35. BETWEEN clause ────────────────────────────────────────────────

    #[test]
    fn test_between_clause() {
        let q = p(r#"RECALL events BETWEEN "2024-01-01" AND "2024-12-31""#);
        match &q.statement {
            CalStatement::Recall(r) => {
                let b = r.between.as_ref().unwrap();
                assert_eq!(b.start, "2024-01-01");
                assert_eq!(b.end, "2024-12-31");
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 36. Error: whitespace-only input ─────────────────────────────────

    #[test]
    fn test_error_whitespace_only_input() {
        let err = pe("   \t\n  ");
        assert!(matches!(err, CalError::EmptyQuery { .. }));
    }

    // ── 37. Error: all destructive keywords produce clear errors ─────────

    #[test]
    fn test_error_all_destructive_keywords() {
        let keywords = [
            "DELETE", "DROP", "FORGET", "ERASE", "DESTROY", "PURGE", "TRUNCATE", "INSERT",
            "CREATE", "WRITE", "STORE",
        ];
        for kw in &keywords {
            let input = format!("{} facts WHERE subject = \"john\"", kw);
            let result = parse(&input);
            assert!(result.is_err(), "'{}' should produce a parse error", kw);
            let err = result.unwrap_err();
            assert!(
                err.suggestion().is_some(),
                "'{}' error should have a suggestion",
                kw
            );
        }
    }

    // ── 38. OR condition produces a warning ──────────────────────────────

    #[test]
    fn test_where_or_produces_warning() {
        let q = p(r#"RECALL facts WHERE subject = "john" OR subject = "bob""#);
        // The parser should emit a warning about OR being partially supported.
        // The query should still parse successfully.
        assert!(matches!(q.statement, CalStatement::Recall(_)));
        // Warnings are collected in the CalQuery.warnings field.
        // (This test mainly verifies it parses without errors.)
    }

    // ── 39. Query length boundary tests ─────────────────────────────────

    #[test]
    fn test_query_well_under_max_length_succeeds() {
        // A query well under MAX_QUERY_LENGTH should parse fine.
        let input = format!("RECALL facts ABOUT \"{}\"", "a".repeat(100));
        assert!(input.len() < MAX_QUERY_LENGTH);
        let result = parse(&input);
        assert!(result.is_ok(), "query under MAX_QUERY_LENGTH should parse");
    }

    #[test]
    fn test_query_over_max_length_rejected() {
        // A query exceeding MAX_QUERY_LENGTH (65536) must be rejected.
        let input = format!("RECALL facts ABOUT \"{}\"", "a".repeat(66_000));
        assert!(input.len() > MAX_QUERY_LENGTH);
        let result = parse(&input);
        assert!(result.is_err(), "query over MAX_QUERY_LENGTH should fail");
        assert!(matches!(result.unwrap_err(), CalError::QueryTooLong { .. }));
    }

    #[test]
    fn test_max_query_length_constant_is_65536() {
        assert_eq!(MAX_QUERY_LENGTH, 65_536);
    }

    // SEC (finding #7, CWE-674): the recursion guard MUST refuse input that
    // exceeds MAX_NESTING_DEPTH. We use parenthesised condition groups inside
    // a WHERE clause — `parse_condition_primary` calls `enter_nesting` for each
    // paren level. With depth 6, opening 20 parens must trip NestingTooDeep
    // long before any pathological input can deplete the worker thread stack.
    #[test]
    fn test_nesting_depth_overflow_is_rejected() {
        let mut input = String::from("RECALL facts WHERE ");
        for _ in 0..20 {
            input.push('(');
        }
        input.push_str("confidence > 0.5");
        for _ in 0..20 {
            input.push(')');
        }
        let result = parse(&input);
        assert!(
            matches!(result, Err(CalError::NestingTooDeep { .. })),
            "deeply nested parens must return NestingTooDeep, got {:?}",
            result
        );
    }

    #[test]
    fn test_nesting_depth_constant_matches_spec() {
        // Text parser and JSON pre-validator must share one limit so a
        // query that parses via one wire format also parses via the other.
        const _: () = assert!(
            MAX_NESTING_DEPTH == 8,
            "MAX_NESTING_DEPTH must remain 8 to stay in sync with json.rs"
        );
    }

    #[test]
    fn test_query_32kb_parses_successfully() {
        // 32KB queries (e.g. agent system prompts) must be accepted.
        // Use a spawned thread with larger stack — debug-mode recursive descent
        // on 32KB strings exceeds the default 8MB test thread stack.
        let result = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024) // 16 MB
            .spawn(|| {
                let query_text = format!("RECALL facts ABOUT \"{}\"", "a".repeat(32_000));
                assert!(query_text.len() > 32_000);
                assert!(query_text.len() < MAX_QUERY_LENGTH);
                let result = parse(&query_text);
                assert!(result.is_ok(), "32KB query should parse successfully");
            })
            .expect("spawn thread")
            .join();
        result.expect("32KB parse thread panicked");
    }

    // ── 40. All 10 grain types parse (plural) ────────────────────────────

    #[test]
    fn test_all_grain_type_plurals_parse() {
        let types = [
            "facts",
            "events",
            "states",
            "workflows",
            "tools",
            "observations",
            "goals",
            "reasonings",
            "consensuses",
            "consents",
        ];
        for gt in &types {
            let input = format!("RECALL {}", gt);
            let result = parse(&input);
            assert!(
                result.is_ok(),
                "RECALL {} should parse successfully, got: {:?}",
                gt,
                result.unwrap_err()
            );
        }
    }

    // ── 41. Case insensitivity throughout the parser ─────────────────────

    #[test]
    fn test_parser_case_insensitive_keywords() {
        // Mix of upper/lower/mixed case should all parse.
        let inputs = [
            "recall facts",
            "Recall Facts",
            "RECALL FACTS",
            "rEcAlL fAcTs",
        ];
        for input in &inputs {
            let result = parse(input);
            assert!(
                result.is_ok(),
                "'{}' should parse case-insensitively",
                input
            );
        }
    }

    // ── 42. OMS 1.1 → 1.2 renamed types produce helpful errors ──────────

    #[test]
    fn test_error_oms_1_1_renamed_types() {
        // "beliefs" (old name for "facts") should give a suggestion.
        let err = pe("RECALL beliefs");
        match err {
            CalError::UnknownGrainType {
                found, suggestion, ..
            } => {
                assert_eq!(found, "beliefs");
                assert!(
                    suggestion.as_ref().unwrap().contains("facts"),
                    "suggestion for 'beliefs' should mention 'facts'"
                );
            }
            other => panic!("expected UnknownGrainType, got {:?}", other),
        }
    }

    // ========================================================================
    // Phase 2 tests — new parser syntax
    // ========================================================================

    // ── 43. ASSEMBLE with multi-source FROM ─────────────────────────────

    #[test]
    fn test_assemble_multi_source() {
        let q = p(
            r#"ASSEMBLE "context" FROM recent: (RECALL facts RECENT 5), background: (RECALL events RECENT 10)"#,
        );
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.topic, "context");
                assert_eq!(a.context_name.as_deref(), Some("context"));
                let sources = a.sources.as_ref().expect("should have named sources");
                assert_eq!(sources.len(), 2);
                assert_eq!(sources[0].label, "recent");
                assert_eq!(sources[1].label, "background");
                assert!(matches!(*sources[0].query, CalStatement::Recall(_)));
                assert!(matches!(*sources[1].query, CalStatement::Recall(_)));
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── #609: per-source WITH inside ASSEMBLE source parens ─────────────

    /// Extract the named sources from an ASSEMBLE query, panicking otherwise.
    fn assemble_sources(q: &CalQuery) -> Vec<NamedSource> {
        match &q.statement {
            CalStatement::Assemble(a) => a
                .sources
                .as_ref()
                .expect("should have named sources")
                .clone(),
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    #[test]
    fn test_assemble_per_source_with_annotate_relative_time() {
        let q = p(
            r#"ASSEMBLE "ctx" FROM messages: (RECALL events RECENT 5 WITH annotate_relative_time)"#,
        );
        let sources = assemble_sources(&q);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].with_options.len(), 1);
        assert!(matches!(
            sources[0].with_options[0],
            WithOption::AnnotateRelativeTime
        ));
    }

    #[test]
    fn test_assemble_per_source_with_rerank() {
        let q = p(r#"ASSEMBLE "ctx" FROM k: (RECALL facts LIMIT 10 WITH rerank)"#);
        let sources = assemble_sources(&q);
        assert!(matches!(
            sources[0].with_options.as_slice(),
            [WithOption::Rerank { .. }]
        ));
    }

    #[test]
    fn test_assemble_per_source_with_conflict_resolution() {
        let q =
            p(r#"ASSEMBLE "ctx" FROM u: (RECALL facts ABOUT "alice" WITH conflict_resolution)"#);
        let sources = assemble_sources(&q);
        assert!(matches!(
            sources[0].with_options.as_slice(),
            [WithOption::ConflictResolution]
        ));
    }

    #[test]
    fn test_assemble_per_source_with_dedup() {
        let q = p(r#"ASSEMBLE "ctx" FROM h: (RECALL tools RECENT 5 WITH dedup)"#);
        let sources = assemble_sources(&q);
        assert!(matches!(
            sources[0].with_options.as_slice(),
            [WithOption::Dedup { .. }]
        ));
    }

    #[test]
    fn test_assemble_per_source_with_min_score() {
        let q = p(r#"ASSEMBLE "ctx" FROM k: (RECALL facts LIMIT 10 WITH min_score(0.5))"#);
        let sources = assemble_sources(&q);
        assert!(matches!(
            sources[0].with_options.as_slice(),
            [WithOption::MinScore { .. }]
        ));
    }

    #[test]
    fn test_assemble_per_source_with_query_expansion() {
        let q = p(r#"ASSEMBLE "ctx" FROM k: (RECALL facts LIMIT 10 WITH query_expansion)"#);
        let sources = assemble_sources(&q);
        assert!(matches!(
            sources[0].with_options.as_slice(),
            [WithOption::QueryExpansion]
        ));
    }

    #[test]
    fn test_assemble_per_source_with_recency_weight() {
        let q = p(r#"ASSEMBLE "ctx" FROM m: (RECALL events RECENT 5 WITH recency_weight(0.7))"#);
        let sources = assemble_sources(&q);
        assert!(matches!(
            sources[0].with_options.as_slice(),
            [WithOption::RecencyWeight { .. }]
        ));
    }

    #[test]
    fn test_assemble_per_source_with_hyde() {
        let q = p(r#"ASSEMBLE "ctx" FROM k: (RECALL facts LIMIT 10 WITH hyde)"#);
        let sources = assemble_sources(&q);
        assert!(matches!(
            sources[0].with_options.as_slice(),
            [WithOption::Hyde]
        ));
    }

    /// Q1: inside-paren options come first, then outside-paren options.
    #[test]
    fn test_assemble_per_source_with_inside_and_outside_merge_order() {
        let q = p(r#"ASSEMBLE "ctx" FROM k: (RECALL facts LIMIT 10 WITH rerank) WITH dedup"#);
        let sources = assemble_sources(&q);
        assert_eq!(sources[0].with_options.len(), 2);
        // Inside-paren first (rerank), then outside-paren (dedup).
        assert!(matches!(
            sources[0].with_options[0],
            WithOption::Rerank { .. }
        ));
        assert!(matches!(
            sources[0].with_options[1],
            WithOption::Dedup { .. }
        ));
    }

    /// Inside-paren WITH on a set-op source: the UNION tail is consumed before
    /// the WITH.
    #[test]
    fn test_assemble_per_source_with_on_set_op_source() {
        let q =
            p(r#"ASSEMBLE "ctx" FROM combined: (RECALL facts UNION RECALL events WITH rerank)"#);
        let sources = assemble_sources(&q);
        assert_eq!(sources.len(), 1);
        assert!(
            matches!(*sources[0].query, CalStatement::SetOp(_)),
            "set-op tail should be consumed into the query, got {:?}",
            sources[0].query
        );
        assert!(matches!(
            sources[0].with_options.as_slice(),
            [WithOption::Rerank { .. }]
        ));
    }

    /// Unknown option inside parens warns (CAL-W004) and is skipped, matching
    /// the outside-paren behavior.
    #[test]
    fn test_assemble_per_source_with_unknown_option_warns() {
        let q = p(r#"ASSEMBLE "ctx" FROM k: (RECALL facts LIMIT 10 WITH bogus_option)"#);
        let sources = assemble_sources(&q);
        assert!(
            sources[0].with_options.is_empty(),
            "unknown option must be skipped, not pushed"
        );
        assert!(
            q.warnings.iter().any(|w| w.code() == "CAL-W004"),
            "expected CAL-W004 warning, got {:?}",
            q.warnings
        );
    }

    /// The ticket_context example from issue #609 parses without error, with
    /// each per-source WITH landing on its own source. Bound params (`$user`,
    /// `$session`) and `status IS OPEN` are shown in their runtime-resolved /
    /// parseable form (`ABOUT` + `WHERE` take string literals in the current
    /// grammar — unrelated to the #609 per-source-WITH change); the per-source
    /// WITH placement under test is preserved verbatim.
    #[test]
    fn test_assemble_issue_609_ticket_context_example() {
        let q = p(r#"ASSEMBLE "ticket_context"
  FROM
    task:      (RECALL goals ABOUT "session-1" WHERE status = "open" LIMIT 1),
    messages:  (RECALL events ABOUT "alice" RECENT 5 WITH annotate_relative_time),
    knowledge: (RECALL facts WHERE tags INCLUDE ["product","support"] LIMIT 10 WITH rerank),
    user:      (RECALL facts ABOUT "alice" WHERE relation IS PREFERENCE WITH conflict_resolution),
    history:   (RECALL tools ABOUT "alice" RECENT 5 WITH dedup)
  BUDGET 2500 tokens
  WITH provenance
  FORMAT TEMPLATE "ticket_brief""#);
        let sources = assemble_sources(&q);
        assert_eq!(sources.len(), 5);
        // task: no per-source WITH.
        assert_eq!(sources[0].label, "task");
        assert!(sources[0].with_options.is_empty());
        // messages: annotate_relative_time.
        assert_eq!(sources[1].label, "messages");
        assert!(matches!(
            sources[1].with_options.as_slice(),
            [WithOption::AnnotateRelativeTime]
        ));
        // knowledge: rerank.
        assert_eq!(sources[2].label, "knowledge");
        assert!(matches!(
            sources[2].with_options.as_slice(),
            [WithOption::Rerank { .. }]
        ));
        // user: conflict_resolution.
        assert_eq!(sources[3].label, "user");
        assert!(matches!(
            sources[3].with_options.as_slice(),
            [WithOption::ConflictResolution]
        ));
        // history: dedup.
        assert_eq!(sources[4].label, "history");
        assert!(matches!(
            sources[4].with_options.as_slice(),
            [WithOption::Dedup { .. }]
        ));
        // Top-level WITH provenance still attaches to the query.
        assert!(
            q.with_options
                .iter()
                .any(|w| matches!(w, WithOption::Provenance)),
            "top-level provenance should be in query with_options: {:?}",
            q.with_options
        );
    }

    // ── 44. ASSEMBLE with BUDGET ────────────────────────────────────────

    #[test]
    fn test_assemble_budget() {
        let q = p(r#"ASSEMBLE "summary" FROM (RECALL facts RECENT 10) BUDGET 2000"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.topic, "summary");
                let budget = a.budget.as_ref().expect("should have budget");
                assert_eq!(budget.tokens, 2000);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── 45. ASSEMBLE with PRIORITY ──────────────────────────────────────

    #[test]
    fn test_assemble_priority() {
        let q = p(
            r#"ASSEMBLE "ctx" FROM recent: (RECALL facts RECENT 5), bg: (RECALL events RECENT 10) PRIORITY recent: 0.8, bg: 0.2"#,
        );
        match &q.statement {
            CalStatement::Assemble(a) => {
                let priority = a.priority.as_ref().expect("should have priority");
                assert_eq!(priority.len(), 2);
                assert_eq!(priority[0].label, "recent");
                assert!((priority[0].weight - 0.8).abs() < f64::EPSILON);
                assert_eq!(priority[1].label, "bg");
                assert!((priority[1].weight - 0.2).abs() < f64::EPSILON);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── 46. ASSEMBLE with FORMAT ────────────────────────────────────────

    #[test]
    fn test_assemble_format() {
        let q = p(r#"ASSEMBLE "summary" FROM (RECALL facts RECENT 10) FORMAT markdown"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.format, Some(FormatClause::Single(FormatSpec::Markdown)));
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── 47. ASSEMBLE with WITH options ──────────────────────────────────

    #[test]
    fn test_assemble_with_options() {
        let q = p(r#"ASSEMBLE "summary" FROM (RECALL facts RECENT 10) WITH dedup(subject)"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.assemble_with.len(), 1);
                let AssembleWithOption::Dedup { field } = &a.assemble_with[0];
                assert_eq!(field.as_deref(), Some("subject"));
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── 48. ASSEMBLE with dedup (no field) ──────────────────────────────

    #[test]
    fn test_assemble_with_dedup_no_field() {
        let q = p(r#"ASSEMBLE "summary" FROM (RECALL facts RECENT 10) WITH dedup"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.assemble_with.len(), 1);
                let AssembleWithOption::Dedup { field } = &a.assemble_with[0];
                assert_eq!(*field, None);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── 49. HISTORY WHERE (Phase 2 triple-based) ────────────────────────

    #[test]
    fn test_history_where() {
        let q = p(r#"HISTORY WHERE subject = "john" AND relation = "likes""#);
        match &q.statement {
            CalStatement::History(h) => {
                assert!(h.hash.is_empty(), "hash should be empty for WHERE-based");
                assert!(h.where_clause.is_some(), "should have where_clause");
            }
            other => panic!("expected History, got {:?}", other),
        }
    }

    // ── 50. HISTORY with DIFF ───────────────────────────────────────────

    #[test]
    fn test_history_diff() {
        let q = p("HISTORY sha256:abc123def456 DIFF sha256:def789abc012");
        match &q.statement {
            CalStatement::History(h) => {
                assert_eq!(h.hash, "abc123def456");
                assert_eq!(h.diff_target.as_deref(), Some("def789abc012"));
            }
            other => panic!("expected History, got {:?}", other),
        }
    }

    // ── 51. HISTORY WHERE with DIFF ─────────────────────────────────────

    #[test]
    fn test_history_where_with_diff() {
        let q =
            p(r#"HISTORY WHERE subject = "john" AND relation = "likes" DIFF sha256:abc123def456"#);
        match &q.statement {
            CalStatement::History(h) => {
                assert!(h.hash.is_empty());
                assert!(h.where_clause.is_some());
                assert_eq!(h.diff_target.as_deref(), Some("abc123def456"));
            }
            other => panic!("expected History, got {:?}", other),
        }
    }

    // ── 52. DESCRIBE CAPABILITIES ───────────────────────────────────────

    #[test]
    fn test_describe_capabilities() {
        let q = p("DESCRIBE capabilities");
        match &q.statement {
            CalStatement::Describe(d) => {
                assert!(matches!(d.target, DescribeTarget::Capabilities));
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    // ── 53. DESCRIBE SERVER ─────────────────────────────────────────────

    #[test]
    fn test_describe_server() {
        let q = p("DESCRIBE server");
        match &q.statement {
            CalStatement::Describe(d) => {
                assert!(matches!(d.target, DescribeTarget::Server));
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    // ── 54. DESCRIBE FIELDS ─────────────────────────────────────────────

    #[test]
    fn test_describe_fields() {
        let q = p("DESCRIBE fields");
        match &q.statement {
            CalStatement::Describe(d) => match &d.target {
                DescribeTarget::Fields(gt) => {
                    assert!(gt.is_none(), "bare FIELDS should have no grain type");
                }
                other => panic!("expected Fields, got {:?}", other),
            },
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    // ── 55. DESCRIBE FIELDS facts ─────────────────────────────────────

    #[test]
    fn test_describe_fields_facts() {
        let q = p("DESCRIBE fields facts");
        match &q.statement {
            CalStatement::Describe(d) => match &d.target {
                DescribeTarget::Fields(gt) => {
                    assert_eq!(*gt, Some(GrainTypePlural::Facts));
                }
                other => panic!("expected Fields, got {:?}", other),
            },
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    // ── 56. DESCRIBE TEMPLATES ──────────────────────────────────────────

    #[test]
    fn test_describe_templates() {
        let q = p("DESCRIBE templates");
        match &q.statement {
            CalStatement::Describe(d) => {
                assert!(matches!(d.target, DescribeTarget::Templates));
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    // ── 57. DESCRIBE GRAMMAR ────────────────────────────────────────────

    #[test]
    fn test_describe_grammar() {
        let q = p("DESCRIBE grammar");
        match &q.statement {
            CalStatement::Describe(d) => {
                assert!(matches!(d.target, DescribeTarget::Grammar));
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    // ── 58. COALESCE with braces (Phase 2) ──────────────────────────────

    #[test]
    fn test_coalesce_braces() {
        let q = p(
            r#"COALESCE { RECALL facts WHERE subject = "john" } OR { RECALL facts WHERE subject = "bob" }"#,
        );
        match &q.statement {
            CalStatement::Coalesce(c) => {
                assert_eq!(c.branches.len(), 2);
                assert!(matches!(c.branches[0].query, CalStatement::Recall(_)));
                assert!(matches!(c.branches[1].query, CalStatement::Recall(_)));
                assert!(c.else_branch.is_none());
            }
            other => panic!("expected Coalesce, got {:?}", other),
        }
    }

    // ── 59. COALESCE with braces and ELSE ───────────────────────────────

    #[test]
    fn test_coalesce_braces_with_else() {
        let q = p(
            r#"COALESCE { RECALL facts WHERE subject = "john" } OR { RECALL facts WHERE subject = "bob" } ELSE { RECALL facts RECENT 5 }"#,
        );
        match &q.statement {
            CalStatement::Coalesce(c) => {
                assert_eq!(c.branches.len(), 2);
                assert!(c.else_branch.is_some());
                let else_stmt = c.else_branch.as_ref().unwrap();
                assert!(matches!(**else_stmt, CalStatement::Recall(_)));
            }
            other => panic!("expected Coalesce, got {:?}", other),
        }
    }

    // ── 60. COALESCE Phase 1 form stores branches ───────────────────────

    #[test]
    fn test_coalesce_phase1_stores_branches() {
        let q = p(
            r#"COALESCE(RECALL facts WHERE subject = "john", RECALL facts WHERE subject = "bob")"#,
        );
        match &q.statement {
            CalStatement::Coalesce(c) => {
                assert_eq!(
                    c.branches.len(),
                    2,
                    "Phase 1 COALESCE should store branches"
                );
                assert!(matches!(c.branches[0].query, CalStatement::Recall(_)));
                assert!(matches!(c.branches[1].query, CalStatement::Recall(_)));
                assert!(c.else_branch.is_none());
            }
            other => panic!("expected Coalesce, got {:?}", other),
        }
    }

    // ── 61. IS CATEGORY condition ───────────────────────────────────────

    #[test]
    fn test_is_category_preference() {
        let q = p("RECALL facts WHERE relation IS PREFERENCE");
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                match cond {
                    Condition::IsCategory {
                        field, category, ..
                    } => {
                        assert_eq!(field, "relation");
                        assert_eq!(category, "preference");
                    }
                    other => panic!("expected IsCategory, got {:?}", other),
                }
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 62. IS CATEGORY — knowledge ─────────────────────────────────────

    #[test]
    fn test_is_category_knowledge() {
        let q = p("RECALL facts WHERE relation IS KNOWLEDGE");
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                match cond {
                    Condition::IsCategory {
                        field, category, ..
                    } => {
                        assert_eq!(field, "relation");
                        assert_eq!(category, "knowledge");
                    }
                    other => panic!("expected IsCategory, got {:?}", other),
                }
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── 63. IS CATEGORY — all 7 categories parse ────────────────────────

    #[test]
    fn test_is_category_all_variants() {
        let categories = [
            "PREFERENCE",
            "KNOWLEDGE",
            "PERMISSION",
            "INTERACTION",
            "AGENCY",
            "LIFECYCLE",
            "OBSERVATION",
        ];
        for cat in &categories {
            let input = format!("RECALL facts WHERE relation IS {}", cat);
            let result = parse(&input);
            assert!(
                result.is_ok(),
                "IS {} should parse successfully, got: {:?}",
                cat,
                result.unwrap_err()
            );
            let q = result.unwrap();
            match &q.statement {
                CalStatement::Recall(r) => {
                    let cond = &r.where_clause.as_ref().unwrap().condition;
                    assert!(
                        matches!(cond, Condition::IsCategory { .. }),
                        "IS {} should produce IsCategory condition, got {:?}",
                        cat,
                        cond
                    );
                }
                _ => panic!("expected Recall for IS {}", cat),
            }
        }
    }

    // ── 64. ASSEMBLE backward compat — Phase 1 single source ────────────

    #[test]
    fn test_assemble_single_source_still_works() {
        // Phase 1 form should still work with Phase 2 code.
        let q = p(r#"ASSEMBLE "daily_summary" FROM (RECALL facts RECENT 10)"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.topic, "daily_summary");
                assert!(
                    a.sources.is_none(),
                    "single source should have no named sources"
                );
                assert!(a.budget.is_none());
                assert!(a.priority.is_none());
                assert!(a.format.is_none());
                assert!(a.assemble_with.is_empty());
                match &a.from {
                    Source::Query(_) => {}
                    other => panic!("expected Query source, got {:?}", other),
                }
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── 65. ASSEMBLE full Phase 2 kitchen sink ──────────────────────────

    #[test]
    fn test_assemble_full_phase2() {
        let q = p(
            r#"ASSEMBLE "brief" FROM recent: (RECALL facts RECENT 5), bg: (RECALL events RECENT 10) BUDGET 1500 PRIORITY recent: 0.7, bg: 0.3 FORMAT json WITH dedup"#,
        );
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.topic, "brief");
                assert_eq!(a.sources.as_ref().unwrap().len(), 2);
                assert_eq!(a.budget.as_ref().unwrap().tokens, 1500);
                assert_eq!(a.priority.as_ref().unwrap().len(), 2);
                assert_eq!(a.format, Some(FormatClause::Single(FormatSpec::Json)));
                assert_eq!(a.assemble_with.len(), 1);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── 66. ASSEMBLE bug-fix regression tests ─────────────────────────

    // Issue 1: WITH token stealing — `WITH rerank` must not be consumed
    // by the ASSEMBLE-specific WITH parser; it should flow to the top-level
    // WITH parser so `rerank` ends up in `query.with_options`.
    #[test]
    fn test_assemble_with_rerank_not_stolen() {
        let q = p(r#"ASSEMBLE "ctx" FROM (RECALL facts RECENT 10) WITH rerank FORMAT json"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                // rerank is NOT an assemble-specific WITH option.
                assert!(
                    a.assemble_with.is_empty(),
                    "rerank should not be in assemble_with"
                );
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
        // rerank should be in the top-level query with_options.
        assert!(
            q.with_options
                .iter()
                .any(|w| matches!(w, WithOption::Rerank { .. })),
            "rerank should be in query with_options: {:?}",
            q.with_options
        );
    }

    // Issue 1 corollary: WITH dedup should still work for ASSEMBLE-specific
    // WITH options.
    #[test]
    fn test_assemble_with_dedup_still_works() {
        let q = p(r#"ASSEMBLE "ctx" FROM (RECALL facts RECENT 10) WITH dedup(subject)"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.assemble_with.len(), 1);
                let AssembleWithOption::Dedup { field } = &a.assemble_with[0];
                assert_eq!(field.as_deref(), Some("subject"));
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // Issue 2: CAL-E032 — too many sources.
    #[test]
    fn test_assemble_too_many_sources() {
        let input = r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL facts), c: (RECALL facts), d: (RECALL facts), e: (RECALL facts), f: (RECALL facts), g: (RECALL facts), h: (RECALL facts), i: (RECALL facts)"#;
        let err = pe(input);
        assert_eq!(err.code(), "CAL-E032");
    }

    // Issue 2: CAL-E034 — duplicate source labels.
    #[test]
    fn test_assemble_duplicate_labels() {
        let input = r#"ASSEMBLE "ctx" FROM a: (RECALL facts), a: (RECALL events)"#;
        let err = pe(input);
        assert_eq!(err.code(), "CAL-E034");
    }

    // Issue 2: CAL-E035 — PRIORITY references unknown label.
    #[test]
    fn test_assemble_priority_unknown_label() {
        let input =
            r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL events) PRIORITY typo: 0.5"#;
        let err = pe(input);
        assert_eq!(err.code(), "CAL-E035");
    }

    // Issue 2: CAL-E033 — BUDGET zero.
    #[test]
    fn test_assemble_budget_zero() {
        let input = r#"ASSEMBLE "ctx" FROM (RECALL facts) BUDGET 0"#;
        let err = pe(input);
        assert_eq!(err.code(), "CAL-E033");
    }

    // Issue 2: CAL-E033 — BUDGET exceeds max.
    #[test]
    fn test_assemble_budget_exceeds_max() {
        let input = r#"ASSEMBLE "ctx" FROM (RECALL facts) BUDGET 20000"#;
        let err = pe(input);
        assert_eq!(err.code(), "CAL-E033");
    }

    // `context_name = identifier` per spec EBNF: both bare identifier and
    // quoted-string topics must parse.
    #[test]
    fn test_assemble_unquoted_topic_accepted() {
        let q = parse(r#"ASSEMBLE user_context FROM (RECALL facts)"#)
            .expect("bare identifier topic must parse");
        match q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.context_name.as_deref(), Some("user_context"));
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
        // Quoted form must continue to work too.
        let q2 = parse(r#"ASSEMBLE "quoted ctx" FROM (RECALL facts)"#)
            .expect("quoted topic must still parse");
        match q2.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.context_name.as_deref(), Some("quoted ctx"));
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // Issue 4: PRIORITY weight > 1.0 should fail.
    #[test]
    fn test_assemble_priority_weight_too_high() {
        let input = r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL events) PRIORITY a: 1.5"#;
        let err = pe(input);
        assert_eq!(err.code(), "CAL-E002");
        assert!(err.to_string().contains("0.0 and 1.0") || err.to_string().contains("weight"));
    }

    // Issue 4: PRIORITY negative weight should fail.
    #[test]
    fn test_assemble_priority_weight_negative() {
        let input = r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL events) PRIORITY a: -0.3"#;
        // Negative numbers are parsed as an error by parse_number since
        // the lexer does not emit negative literals.
        let err = pe(input);
        // The exact error may be "unexpected token" because `-` is not a number.
        assert!(err.to_string().contains("CAL-E"));
    }

    // Issue 5: FOR clause stored in for_whom field.
    #[test]
    fn test_assemble_for_clause_stored() {
        let q = p(r#"ASSEMBLE "ctx" FOR "john" FROM (RECALL facts)"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.for_whom, Some("john".to_string()));
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // Issue 5: FOR clause absent => for_whom is None.
    #[test]
    fn test_assemble_no_for_clause() {
        let q = p(r#"ASSEMBLE "ctx" FROM (RECALL facts)"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(a.for_whom, None);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // Issue 6: WHERE on multi-source ASSEMBLE should fail.
    #[test]
    fn test_assemble_where_multi_source_rejected() {
        let input =
            r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL events) WHERE confidence >= 0.8"#;
        let err = pe(input);
        assert_eq!(err.code(), "CAL-E002");
        let suggestion = err.suggestion().unwrap_or("");
        assert!(
            suggestion.contains("not supported with multi-source"),
            "suggestion should explain WHERE is not supported: {}",
            suggestion
        );
    }

    // Issue 6: WHERE on single-source ASSEMBLE should still work.
    #[test]
    fn test_assemble_where_single_source_still_works() {
        let q = p(r#"ASSEMBLE "ctx" FROM (RECALL facts) WHERE confidence >= 0.8"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert!(a.where_clause.is_some(), "single-source WHERE should work");
                assert!(a.sources.is_none());
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── Issue 1: BUDGET unit suffix parsing ────────────────────────────

    #[test]
    fn test_assemble_budget_with_tokens_unit() {
        let q = p(r#"ASSEMBLE "ctx" FROM (RECALL facts) BUDGET 2000 tokens"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                let budget = a.budget.as_ref().expect("should have budget");
                assert_eq!(budget.tokens, 2000);
                assert_eq!(budget.unit, BudgetUnit::Tokens);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    #[test]
    fn test_assemble_budget_with_grains_unit() {
        let q = p(r#"ASSEMBLE "ctx" FROM (RECALL facts) BUDGET 50 grains"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                let budget = a.budget.as_ref().expect("should have budget");
                assert_eq!(budget.tokens, 50);
                assert_eq!(budget.unit, BudgetUnit::Grains);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    #[test]
    fn test_assemble_budget_no_unit_defaults_to_tokens() {
        let q = p(r#"ASSEMBLE "ctx" FROM (RECALL facts) BUDGET 100"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                let budget = a.budget.as_ref().expect("should have budget");
                assert_eq!(budget.tokens, 100);
                assert_eq!(budget.unit, BudgetUnit::Tokens);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── Issue 2: PRIORITY ordering syntax ────────────────────────────────

    #[test]
    fn test_assemble_priority_ordering_syntax() {
        let q = p(
            r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL events), c: (RECALL goals) PRIORITY a > b > c"#,
        );
        match &q.statement {
            CalStatement::Assemble(a) => {
                let priority = a.priority.as_ref().expect("should have priority");
                assert_eq!(priority.len(), 3);
                assert_eq!(priority[0].label, "a");
                assert!((priority[0].weight - 1.0).abs() < f64::EPSILON);
                assert_eq!(priority[1].label, "b");
                // 2/3 ≈ 0.6667
                assert!((priority[1].weight - 2.0 / 3.0).abs() < 0.001);
                assert_eq!(priority[2].label, "c");
                // 1/3 ≈ 0.3333
                assert!((priority[2].weight - 1.0 / 3.0).abs() < 0.001);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    #[test]
    fn test_assemble_priority_weighted_syntax_still_works() {
        let q = p(
            r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL events) PRIORITY a: 0.7, b: 0.3"#,
        );
        match &q.statement {
            CalStatement::Assemble(a) => {
                let priority = a.priority.as_ref().expect("should have priority");
                assert_eq!(priority.len(), 2);
                assert_eq!(priority[0].label, "a");
                assert!((priority[0].weight - 0.7).abs() < f64::EPSILON);
                assert_eq!(priority[1].label, "b");
                assert!((priority[1].weight - 0.3).abs() < f64::EPSILON);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    #[test]
    fn test_assemble_priority_ordering_two_labels() {
        let q = p(r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL events) PRIORITY a > b"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                let priority = a.priority.as_ref().expect("should have priority");
                assert_eq!(priority.len(), 2);
                assert_eq!(priority[0].label, "a");
                assert!((priority[0].weight - 1.0).abs() < f64::EPSILON);
                assert_eq!(priority[1].label, "b");
                assert!((priority[1].weight - 0.5).abs() < f64::EPSILON);
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // PRIORITY with a single label and no BUDGET must parse — the colon-only
    // discriminator allows `PRIORITY label FORMAT ...` to fall into the
    // ordering branch rather than the weighted branch (which would expect ":").
    #[test]
    fn test_assemble_priority_single_label_no_budget_parses() {
        let q = p(
            r#"ASSEMBLE "ctx" FOR "topic" FROM s1: (RECALL facts RECENT 5) PRIORITY s1 FORMAT markdown"#,
        );
        match &q.statement {
            CalStatement::Assemble(a) => {
                let priority = a.priority.as_ref().expect("should have priority");
                assert_eq!(priority.len(), 1);
                assert_eq!(priority[0].label, "s1");
                assert!((priority[0].weight - 1.0).abs() < f64::EPSILON);
                assert!(a.format.is_some(), "FORMAT clause should also parse");
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // PRIORITY ordering form (`a > b`) followed by FORMAT, with no BUDGET.
    #[test]
    fn test_assemble_priority_ordering_then_format_no_budget() {
        let q = p(
            r#"ASSEMBLE "ctx" FROM a: (RECALL facts), b: (RECALL events) PRIORITY a > b FORMAT markdown"#,
        );
        match &q.statement {
            CalStatement::Assemble(a) => {
                let priority = a.priority.as_ref().expect("should have priority");
                assert_eq!(priority.len(), 2);
                assert_eq!(priority[0].label, "a");
                assert_eq!(priority[1].label, "b");
                assert!(a.format.is_some(), "FORMAT clause should parse");
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // `tags INCLUDE [...]` must desugar to `Condition::In` so the executor
    // routes it through the set-condition path (params.tags) instead of
    // falling through `Condition::Comparison`'s wildcard arm and emitting a
    // bogus CAL-W010 warning.
    #[test]
    fn test_where_tags_include_desugars_to_in() {
        let q = p(r#"RECALL facts WHERE tags INCLUDE ["strategy", "platform"]"#);
        match &q.statement {
            CalStatement::Recall(r) => {
                let cond = &r.where_clause.as_ref().unwrap().condition;
                match cond {
                    Condition::In { field, values, .. } => {
                        assert_eq!(field, "tags");
                        assert_eq!(values.len(), 2);
                    }
                    other => panic!("expected Condition::In, got {:?}", other),
                }
            }
            _ => panic!("expected Recall"),
        }
    }

    // ── Issue 7: BUDGET u64→u32 overflow ─────────────────────────────────

    #[test]
    fn test_assemble_budget_u32_overflow() {
        // 5_000_000_000 > u32::MAX (4_294_967_295), should fail with CAL-E033.
        let input = r#"ASSEMBLE "ctx" FROM (RECALL facts) BUDGET 5000000000"#;
        let err = pe(input);
        assert_eq!(err.code(), "CAL-E033");
    }

    // ── Multi-format tests (CAL spec v1.0.1, Section 10.1.1) ─────────

    #[test]
    fn test_format_multi_two_formats() {
        let q = p("RECALL facts FORMAT [markdown, json]");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af(FormatSpec::Markdown),
                af(FormatSpec::Json)
            ]))
        );
    }

    #[test]
    fn test_format_multi_single_element_list() {
        // FORMAT [json] returns Multi with one element, not Single.
        let q = p("RECALL facts FORMAT [json]");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![af(FormatSpec::Json)]))
        );
    }

    #[test]
    fn test_format_multi_all_types() {
        let q = p("RECALL facts FORMAT [json, markdown, yaml, text, sml]");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af(FormatSpec::Json),
                af(FormatSpec::Markdown),
                af(FormatSpec::Yaml),
                af(FormatSpec::Text),
                af(FormatSpec::Sml),
            ]))
        );
    }

    #[test]
    fn test_format_multi_deduplicates() {
        let q = p("RECALL facts FORMAT [json, json, markdown]");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af(FormatSpec::Json),
                af(FormatSpec::Markdown)
            ]))
        );
    }

    #[test]
    fn test_format_multi_empty_list_errors() {
        let result = crate::parser::parse("RECALL facts FORMAT []");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("at least one format type"));
    }

    #[test]
    fn test_format_multi_too_many_errors() {
        let result = crate::parser::parse(
            "RECALL facts FORMAT [json, markdown, yaml, text, sml, toon]",
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E110");
    }

    #[test]
    fn test_format_single_unchanged() {
        // Backward compat: single format still works.
        let q = p("RECALL facts FORMAT markdown");
        assert_eq!(q.format, Some(FormatClause::Single(FormatSpec::Markdown)));
    }

    #[test]
    fn test_assemble_format_multi() {
        let q = p(r#"ASSEMBLE "ctx" FROM (RECALL facts RECENT 5) FORMAT [json, toon]"#);
        match &q.statement {
            CalStatement::Assemble(a) => {
                assert_eq!(
                    a.format,
                    Some(FormatClause::Multi(vec![
                        af(FormatSpec::Json),
                        af(FormatSpec::Toon)
                    ]))
                );
            }
            other => panic!("expected Assemble, got {:?}", other),
        }
    }

    // ── Format aliases ────────────────────────────────────────────────

    #[test]
    fn test_format_alias_single_alias() {
        let q = p("RECALL facts FORMAT [json AS customers]");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![af_as(
                FormatSpec::Json,
                "customers"
            )]))
        );
    }

    /// Regression: several natural alias words (DATA, READABLE, COMPACT) are
    /// keyword tokens, so an alias parsed as a bare identifier rejected the
    /// CAL reference manual's own example. An alias is a label, not an ident.
    #[test]
    fn test_format_alias_may_be_a_reserved_word() {
        let q = p("RECALL facts FORMAT [json AS data, markdown AS readable]");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af_as(FormatSpec::Json, "data"),
                af_as(FormatSpec::Markdown, "readable"),
            ]))
        );
        let q = p("RECALL facts FORMAT [sml AS compact, json AS raw]");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af_as(FormatSpec::Sml, "compact"),
                af_as(FormatSpec::Json, "raw"),
            ]))
        );
    }

    /// Invariant 3: the destructive vocabulary stays out of CAL text
    /// everywhere it can appear, including as an output alias.
    #[test]
    fn test_format_alias_rejects_destructive_words() {
        // "grant"/"revoke" left this list in CAL 1.3 — they are DCL
        // keywords now, not blocked idents.
        for word in ["delete", "erase", "truncate", "insert", "create"] {
            let q = format!("RECALL facts FORMAT [json AS {word}]");
            parse(&q).expect_err(&format!("{word} must not be usable as an alias"));
        }
    }

    /// A duplicate alias is still an error, reserved word or not.
    #[test]
    fn test_format_alias_reserved_word_duplicate_still_rejected() {
        let e = pe("RECALL facts FORMAT [json AS data, markdown AS data]");
        assert_eq!(e.code(), "CAL-E113", "expected duplicate-key error, got {e:?}");
    }

    // ── RECALL * ──────────────────────────────────────────────────────

    /// `RECALL *` is the documented explicit spelling of "any grain type"
    /// (CAL reference §4) and must mean the same as omitting the type.
    #[test]
    fn test_recall_star_is_all_grain_types() {
        let starred = p(r#"RECALL * WHERE namespace = "caller""#);
        let omitted = p(r#"RECALL WHERE namespace = "caller""#);
        match (&starred.statement, &omitted.statement) {
            (CalStatement::Recall(a), CalStatement::Recall(b)) => {
                assert_eq!(a.grain_type, GrainTypePlural::All);
                assert_eq!(a.grain_type, b.grain_type);
            }
            other => panic!("expected two Recall statements, got {other:?}"),
        }
    }

    #[test]
    fn test_recall_star_through_a_pipeline() {
        let q = p(r#"RECALL * WHERE namespace = "caller" | COUNT"#);
        match &q.statement {
            CalStatement::Recall(r) => assert_eq!(r.grain_type, GrainTypePlural::All),
            other => panic!("expected Recall, got {other:?}"),
        }
        assert_eq!(q.pipeline.len(), 1);
    }

    #[test]
    fn test_format_alias_mixed() {
        let q = p("RECALL facts FORMAT [json AS customers, markdown]");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af_as(FormatSpec::Json, "customers"),
                af(FormatSpec::Markdown),
            ]))
        );
    }

    #[test]
    fn test_format_alias_template() {
        let q = p(
            r#"RECALL facts FORMAT [json AS customers, TEMPLATE "{{subject}}: {{object}}" AS summary]"#,
        );
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af_as(FormatSpec::Json, "customers"),
                af_as(
                    FormatSpec::Template {
                        template: "{{subject}}: {{object}}".into()
                    },
                    "summary"
                ),
            ]))
        );
    }

    #[test]
    fn test_format_alias_two_templates_different_aliases() {
        let q = p(
            r#"RECALL facts FORMAT [TEMPLATE "{{subject}}" AS names, TEMPLATE "{{object}}" AS values]"#,
        );
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af_as(
                    FormatSpec::Template {
                        template: "{{subject}}".into()
                    },
                    "names"
                ),
                af_as(
                    FormatSpec::Template {
                        template: "{{object}}".into()
                    },
                    "values"
                ),
            ]))
        );
    }

    #[test]
    fn test_format_alias_duplicate_alias_errors() {
        let err = pe("RECALL facts FORMAT [json AS customers, markdown AS customers]");
        assert_eq!(err.code(), "CAL-E113");
    }

    #[test]
    fn test_format_alias_not_in_unbracketed() {
        // Unbracketed comma-separated multi-format does not support aliases.
        // `AS` after a comma-separated format is not consumed as an alias.
        let q = p("RECALL facts FORMAT json, markdown");
        assert_eq!(
            q.format,
            Some(FormatClause::Multi(vec![
                af(FormatSpec::Json),
                af(FormatSpec::Markdown)
            ]))
        );
    }

    // ── WITH VARS ───────────────────────────────────────────────────────

    #[test]
    fn test_with_vars_basic() {
        let q = p(r#"RECALL facts WITH VARS { "name": "John", "theme": "dark" }"#);
        assert_eq!(q.user_vars.len(), 2);
        assert_eq!(q.user_vars.get("name").unwrap(), "John");
        assert_eq!(q.user_vars.get("theme").unwrap(), "dark");
    }

    #[test]
    fn test_with_vars_empty() {
        let q = p(r#"RECALL facts WITH VARS { }"#);
        assert!(q.user_vars.is_empty());
    }

    #[test]
    fn test_with_vars_single() {
        let q = p(r#"RECALL facts WITH VARS { "key": "value" }"#);
        assert_eq!(q.user_vars.len(), 1);
        assert_eq!(q.user_vars.get("key").unwrap(), "value");
    }

    #[test]
    fn test_with_vars_trailing_comma() {
        let q = p(r#"RECALL facts WITH VARS { "a": "1", "b": "2", }"#);
        assert_eq!(q.user_vars.len(), 2);
    }

    #[test]
    fn test_with_vars_after_format() {
        let q = p(r#"RECALL facts FORMAT json WITH VARS { "x": "y" }"#);
        assert!(q.format.is_some());
        assert_eq!(q.user_vars.get("x").unwrap(), "y");
    }

    #[test]
    fn test_with_vars_after_with_options_and_format() {
        let q = p(r#"RECALL facts WITH superseded FORMAT json WITH VARS { "a": "b" }"#);
        assert!(q.with_options.contains(&WithOption::Superseded));
        assert!(q.format.is_some());
        assert_eq!(q.user_vars.get("a").unwrap(), "b");
    }

    #[test]
    fn test_with_vars_without_format() {
        let q = p(r#"RECALL facts WITH VARS { "key": "val" }"#);
        assert!(q.format.is_none());
        assert_eq!(q.user_vars.get("key").unwrap(), "val");
    }

    #[test]
    fn test_with_vars_too_many() {
        let input = format!(
            r#"RECALL facts WITH VARS {{ {} }}"#,
            (0..11)
                .map(|i| format!(r#""k{}": "v{}""#, i, i))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let err = pe(&input);
        assert_eq!(err.code(), "CAL-E111");
    }

    #[test]
    fn test_with_vars_value_too_large() {
        let big_val = "x".repeat(1025);
        let input = format!(r#"RECALL facts WITH VARS {{ "k": "{}" }}"#, big_val);
        let err = pe(&input);
        assert_eq!(err.code(), "CAL-E112");
    }

    #[test]
    fn test_with_vars_invalid_key_starts_with_digit() {
        let err = pe(r#"RECALL facts WITH VARS { "1bad": "val" }"#);
        assert_eq!(err.code(), "CAL-E002"); // InvalidSyntax for invalid key
    }

    #[test]
    fn test_with_vars_coexists_with_pipeline() {
        let q = p(r#"RECALL facts | LIMIT 5 WITH VARS { "x": "1" }"#);
        assert!(!q.pipeline.is_empty());
        assert_eq!(q.user_vars.get("x").unwrap(), "1");
    }

    #[test]
    fn test_with_vars_exactly_at_limit() {
        let input = format!(
            r#"RECALL facts WITH VARS {{ {} }}"#,
            (0..10)
                .map(|i| format!(r#""k{}": "v{}""#, i, i))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let q = p(&input);
        assert_eq!(q.user_vars.len(), 10, "exactly 10 vars should be allowed");
    }

    #[test]
    fn test_with_vars_value_exactly_at_size_limit() {
        let val = "x".repeat(1024);
        let input = format!(r#"RECALL facts WITH VARS {{ "k": "{}" }}"#, val);
        let q = p(&input);
        assert_eq!(q.user_vars.get("k").unwrap().len(), 1024);
    }

    #[test]
    fn test_with_vars_underscore_prefixed_key() {
        let q = p(r#"RECALL facts WITH VARS { "_private": "value" }"#);
        assert_eq!(q.user_vars.get("_private").unwrap(), "value");
    }

    #[test]
    fn test_with_vars_empty_value() {
        let q = p(r#"RECALL facts WITH VARS { "key": "" }"#);
        assert_eq!(q.user_vars.get("key").unwrap(), "");
    }

    #[test]
    fn test_with_vars_duplicate_key_last_wins() {
        let q = p(r#"RECALL facts WITH VARS { "k": "first", "k": "second" }"#);
        assert_eq!(q.user_vars.len(), 1);
        assert_eq!(q.user_vars.get("k").unwrap(), "second");
    }

    #[test]
    fn test_with_vars_invalid_key_with_hyphen() {
        let err = pe(r#"RECALL facts WITH VARS { "my-key": "val" }"#);
        assert_eq!(err.code(), "CAL-E002");
    }

    #[test]
    fn test_with_vars_invalid_key_empty_string() {
        let err = pe(r#"RECALL facts WITH VARS { "": "val" }"#);
        assert_eq!(err.code(), "CAL-E002");
    }

    // ── ACCUMULATE parser tests ──────────────────────────────────────────

    #[test]
    fn test_accumulate_tip_resolved_basic() {
        let q = p(
            r#"ACCUMULATE fact WHERE subject = "john" relation = "score" ADD importance = 0.5 REASON "bump""#,
        );
        match &q.statement {
            CalStatement::Accumulate(acc) => {
                assert_eq!(acc.grain_type, GrainTypeSingular::Fact);
                match &acc.target {
                    AccumulateTarget::TipResolved {
                        subject,
                        relation,
                        namespace,
                    } => {
                        assert_eq!(subject, "john");
                        assert_eq!(relation, "score");
                        assert!(namespace.is_none());
                    }
                    other => panic!("expected TipResolved, got {:?}", other),
                }
                assert_eq!(acc.add_ops.len(), 1);
                assert_eq!(acc.add_ops[0].field, "importance");
                assert!((acc.add_ops[0].delta - 0.5).abs() < f64::EPSILON);
                assert!(acc.set_ops.is_empty());
                assert_eq!(acc.reason, "bump");
            }
            other => panic!("expected Accumulate, got {:?}", other),
        }
    }

    #[test]
    fn test_accumulate_tip_resolved_with_namespace() {
        let q = p(
            r#"ACCUMULATE fact WHERE subject = "john" relation = "score" namespace = "team1" ADD confidence = 0.1 REASON "ns test""#,
        );
        match &q.statement {
            CalStatement::Accumulate(acc) => match &acc.target {
                AccumulateTarget::TipResolved {
                    subject,
                    relation,
                    namespace,
                } => {
                    assert_eq!(subject, "john");
                    assert_eq!(relation, "score");
                    assert_eq!(namespace.as_deref(), Some("team1"));
                }
                other => panic!("expected TipResolved, got {:?}", other),
            },
            other => panic!("expected Accumulate, got {:?}", other),
        }
    }

    #[test]
    fn test_accumulate_hash_targeted() {
        let q = p(
            r#"ACCUMULATE fact sha256:0000000000000000000000000000000000000000000000000000000000000001 ADD importance = 0.25 REASON "hash test""#,
        );
        match &q.statement {
            CalStatement::Accumulate(acc) => {
                assert_eq!(acc.grain_type, GrainTypeSingular::Fact);
                match &acc.target {
                    AccumulateTarget::Hash { hash } => {
                        assert!(hash.contains(
                            "0000000000000000000000000000000000000000000000000000000000000001"
                        ));
                    }
                    other => panic!("expected Hash target, got {:?}", other),
                }
                assert_eq!(acc.add_ops.len(), 1);
                assert_eq!(acc.reason, "hash test");
            }
            other => panic!("expected Accumulate, got {:?}", other),
        }
    }

    #[test]
    fn test_accumulate_multiple_add_ops() {
        let q = p(
            r#"ACCUMULATE fact WHERE subject = "x" relation = "y" ADD importance = 0.1 ADD confidence = 0.2 REASON "multi""#,
        );
        match &q.statement {
            CalStatement::Accumulate(acc) => {
                assert_eq!(acc.add_ops.len(), 2);
                assert_eq!(acc.add_ops[0].field, "importance");
                assert!((acc.add_ops[0].delta - 0.1).abs() < f64::EPSILON);
                assert_eq!(acc.add_ops[1].field, "confidence");
                assert!((acc.add_ops[1].delta - 0.2).abs() < f64::EPSILON);
            }
            other => panic!("expected Accumulate, got {:?}", other),
        }
    }

    #[test]
    fn test_accumulate_with_set_ops() {
        let q = p(
            r#"ACCUMULATE fact WHERE subject = "x" relation = "y" ADD importance = 0.1 SET object = "new val" REASON "set test""#,
        );
        match &q.statement {
            CalStatement::Accumulate(acc) => {
                assert_eq!(acc.add_ops.len(), 1);
                assert_eq!(acc.set_ops.len(), 1);
                assert_eq!(acc.set_ops[0].field, "object");
                assert_eq!(
                    acc.set_ops[0].value,
                    Value::String {
                        value: "new val".into()
                    }
                );
            }
            other => panic!("expected Accumulate, got {:?}", other),
        }
    }

    #[test]
    fn test_accumulate_because_alias() {
        let q = p(
            r#"ACCUMULATE fact WHERE subject = "x" relation = "y" ADD importance = 1 BECAUSE "alias""#,
        );
        match &q.statement {
            CalStatement::Accumulate(acc) => {
                assert_eq!(acc.reason, "alias");
            }
            other => panic!("expected Accumulate, got {:?}", other),
        }
    }

    #[test]
    fn test_accumulate_negative_delta() {
        let q = p(
            r#"ACCUMULATE fact WHERE subject = "x" relation = "y" ADD importance = -0.3 REASON "decay""#,
        );
        match &q.statement {
            CalStatement::Accumulate(acc) => {
                assert_eq!(acc.add_ops.len(), 1);
                assert!((acc.add_ops[0].delta - (-0.3)).abs() < f64::EPSILON);
            }
            other => panic!("expected Accumulate, got {:?}", other),
        }
    }

    #[test]
    fn test_accumulate_missing_add_ops_error() {
        let err = pe(
            r#"ACCUMULATE fact WHERE subject = "x" relation = "y" SET foo = "bar" REASON "no adds""#,
        );
        assert_eq!(err.code(), "CAL-E080");
    }

    #[test]
    fn test_accumulate_missing_reason_error() {
        let err = pe(r#"ACCUMULATE fact WHERE subject = "x" relation = "y" ADD importance = 0.1"#);
        assert_eq!(err.code(), "CAL-E018");
    }

    #[test]
    fn test_accumulate_missing_target_error() {
        let err = pe(r#"ACCUMULATE fact ADD importance = 0.1 REASON "no target""#);
        assert_eq!(err.code(), "CAL-E002");
    }

    #[test]
    fn test_accumulate_case_insensitive() {
        let q = p(
            r#"accumulate fact WHERE subject = "x" relation = "y" ADD importance = 1 REASON "ci""#,
        );
        assert!(matches!(&q.statement, CalStatement::Accumulate(_)));
    }
}

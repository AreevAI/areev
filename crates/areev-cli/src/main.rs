//! areev — the Areev CLI (brand Areev, package areev, run `areev`).
//!
//! Thin shell over areev-store + areev-cal. One memory = one file.

mod corpus;
mod run_stack;
mod tune;
mod trigger_cli;

use std::collections::HashMap;
use std::process::ExitCode;

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::error::Hash;
use areev_core::types::{ContentBlock, Event, Fact, Grain, TokenUsage, Tool};
use areev_store::{Axis, Areev, Direction};
use areev_loop_adapter::{now_ms, AreevSubstrate};
use areev_loop::{Decision, Engine, ObserverType, Policy, RecStatus, RunOptions, ScopeSet, Severity};

const USAGE: &str = "\
areev — embedded memory engine for AI agents (OMS + CAL on Turso)

USAGE:
  areev <command> [--db <memory.db>] [options]   (-d = --db)
  areev --version | -V                 print the version and exit
  areev help | --help | -h             show this help

COMMANDS:
  init     [--template blank|demo|coding-agent] [--ns NS]   seed a backend +
           print the Claude Code hook snippet (never writes your settings)
  loop     <run|reflect|list|show|approve|reject|apply|rollback|analyzers|policy>
           the governed self-improvement loop (deterministic core; optional verified LLM):
           run    [--min-new N --min-new-errors N --if-stale 6h --format json --quiet]
                  [--model provider:name | --llm-cmd 'CMD']   optional LLM reflection
                  (--model reads the key from $ANTHROPIC_API_KEY/$OPENAI_API_KEY/etc.)
                  [--ground-model provider:name | --ground-cmd 'CMD']  separate
                  grounding backend (defaults to the reflection model)
                  [--analyzer-cmd 'CMD']   register an external analyzer
                  (advisory only — never auto-applies)
           reflect  like run, but re-analyzes the whole memory (ignores the
                  incremental watermark) — a full sweep; same flags as run
           list   [--status pending|all|applied|...] [--fail-on high]  (exit 2 on match)
           show <hash> | approve/reject/apply/rollback <hash> --because \"...\" [--actor A]
           outcomes  the Verify gate: did applied advice hold, or regress?
           [--policy FILE] grants auto-apply (else $AREEV_LOOP_POLICY); `policy` prints it
  add      <subject> <relation> <object>       store a fact (positional)
           [--subject S --relation R --object O] [--ns NS] [--confidence C]
           [--idempotent]   no-op if this exact value is already the head
  record-tool-call --name NAME [--input JSON] --result TEXT [--is-error]
           [--thread ID] [--call-id ID] [--ns NS]   append one tool invocation
  run-manifest --run-id ID --config JSON   persist a reproducible harness
           configuration and its run link in agent:harness
  recall   <subject> | --subject S   [--relation R] [--ns NS] [-k N]
           [--render sml|toon|markdown|plain|json] [--budget TOKENS]
           --ns accepts a prefix scope 'org.*' (= org + its .-descendants);
           writes and destruction never accept patterns
  cal      <QUERY> [--ns NS]          execute a CAL statement
  corpus   --select '<READ CAL>' [--out FILE] [--recipient ID]
                                      stream governed OpenAI chat
                                      JSONL and record its provenance registry
  tune     --cmd 'TRAINER' --evalset HASH
           (--select '<READ CAL>' --out FILE | --corpus FILE --manifest HASH)
           [--recipient ID] [--timeout-secs N]
                                      hand the corpus to YOUR trainer (JSON on
                                      stdio; Areev never trains) and register
                                      the returned adapter with full lineage;
                                      promotion then rides the loop:
                                      `loop run` proposes, `eval run --model`
                                      gates, `loop apply --gating-run` admits
  search   --query TEXT [--subject S] [-k N]   hybrid recall (BM25 + structural, RRF)
  history  --subject S --relation R [--ns NS]
  provenance <source-hash>            grains distilled from a source (reverse)
  forks                               open forks (>1 head for a subject+relation)
  merge    --subject S --relation R --object O   close a fork with a resolved value
  subject-report <subject> [--ns NS] [--text-mentions] [--out FILE] [--bundle FILE]
                                      DSAR read (GDPR Art. 15/20): everything
                                      forget-subject WOULD erase — exact +
                                      partition keys, history — as JSONL
                                      (stdout or --out), and optionally a
                                      portable .mgb bundle (--bundle)
  forget-subject <subject> [--ns NS] [--text-mentions] --yes   erase EVERY
                                      grain referencing an identity — exact +
                                      partition keys (pat, pat#visit1), history
                                      included, + its dictionary entries;
                                      --text-mentions also erases grains whose
                                      indexed text mentions the identity;
                                      replicates as tombstones
  purge-older-than <days> [--ns NS] [--type event] --yes   retention sweep:
                                      erase grains older than N days
                                      (--ns \"\" sweeps every namespace)
  retention <set|list|clear|sweep> [--days N] [--ns NS] [--type event]
           [--because \"why\"] [--yes]  declarative storage limitation: the
                                      policy is a file-truth that travels
                                      with the memory; `set` declares,
                                      `sweep --yes` enforces (audited)
  trigger  add --type KIND --workflow HASH --because \"why\"
           [--context-query SPEC]     a saved query the evaluator runs at
                                      fire time; its result rides into the
                                      run input as `context` (how a
                                      trigger-started run reads its own
                                      memory on the embedded backend).
                                      SPEC is `name` or `name($p = /ptr,
                                      ...)` — pointers resolve against the
                                      firing item's payload (fail-closed)
           [--interval SECS | --cron EXPR | --at MS] [--observer NAME]
           [--scope S] [--dedup-key PTR] [--catchup last|none|all]
           [--where EXPR] [--members ALIAS=HASH,...] [--correlate PTR]
           [--window 10m]
                                      declare a trigger. Like retention, the
                                      declaration is a file-truth that
                                      travels with the memory and is enforced
                                      separately. --where is CAL WHERE syntax:
                                      it selects grains for a `memory` trigger
                                      and gates member aliases for a
                                      `composite` one
  trigger  run [--id T] [--connector-cmd CMD] [--tool-cmd CMD] [--dry-run]
           [--lease SECS] [--max-items N] [--credential NAME=ENV_VAR|cmd:CMD|vault:P#F]
           [--credential-ttl SECS] [--resolver-env VAR,...]
           [--allow-executor HEX,...] [--sandbox-cmd CMD] [--executor-cache DIR]
           [--executor-timeout SECS] [--model SPEC] [--base-url URL] [--key-env VAR]
           [--max-tokens N] [--max-usd USD] [--max-wall-ms MS] [--ask-ttl SECS]
                                      evaluate once and exit — the cadence is
                                      data in the memory, so the heartbeat can
                                      be coarse. Safe to invoke concurrently.
                                      A firing gets the SAME runner `run start`
                                      builds: the executor pin, the sandbox and
                                      the model all reach it, so a plan that
                                      runs by hand runs on a heartbeat. Every
                                      one of those also reads its $AREEV_RUN_*
                                      variable, because a heartbeat is a cron
                                      line, not an interactive command.
                                      With none of --tool-cmd/--allow-executor/
                                      --model it ingests without executing;
                                      --dry-run touches nothing.
                                      --credential names an env var whose value
                                      the egress broker attaches on the way out,
                                      so the connector never holds the token.
                                      cmd:/vault: MINT one per call instead —
                                      what an unattended heartbeat wants, since
                                      a token minted at boot expires by morning.
                                      The budget flags are the ones `run start`
                                      takes, and bound the runs a firing starts
  trigger  render --target cron|launchd|systemd|k8s-cronjob
                                      emit heartbeat config for infrastructure
                                      you already run; never creates anything
  trigger  deliver --id T [< payload.json] [--tool-cmd CMD] [budget flags]
                                      hand a webhook/manual payload to Areev.
                                      The host owns the listener — Areev never
                                      opens a port. Idempotent on the dedup key.
                                      Takes the same runner and budget flags as
                                      `trigger run`
  trigger  list | show <T> | status   what is declared, and what has actually
                                      fired (a trigger that never fired is
                                      reported, not silent). `show` also
                                      surfaces the cursor and, for a
                                      superseded declaration, the state key
                                      its evaluation actually lives under
  trigger  pause <T> --because \"why\" | resume <T> --because \"why\"
                                      operational brake; unlike the
                                      declaration's own enabled flag it stays
                                      on this host and does not replicate
  blob     put <FILE> | put --stdin   store bytes in the content-addressed
                                      blob store; prints the cas:// URI
                                      (idempotent — the address IS the content)
           get <cas-uri>              write those bytes to stdout, hash-verified.
                                      Reads the .blobs sidecar WITHOUT opening
                                      the memory, so a --tool-cmd subprocess can
                                      fetch an attachment while its run holds
                                      the single writer (an encrypted memory
                                      still needs --passphrase-env)
  blobs encrypt                       encrypt CAS attachments written before
                                      sidecar encryption landed (needs the
                                      memory's key; new blobs are sealed on
                                      write). Idempotent
  audit export [--since MS] [--until MS] [--out FILE]   accountability
                                      evidence as JSONL: every destructive
                                      op (who/what/why/how many) + the loop
                                      lifecycle chain, hash-chain verified
  novelty  --text T [--subject S] [--relation R] [-k N]   nearest existing grains
                                      (paraphrase check; needs --embed-cmd)
  log      [--since OP] [--limit N]   op-log (change feed)
  bundle   --out FILE [--since OP]    incremental backup (git-shaped)
  import   --bundle FILE              apply a bundle (fast-forward)
  migrate  --from SRC --file PATH [--history PATH]   import another system's
           export: mem0 | mem0-history | langgraph | letta | letta-archival |
           zep | basic-memory (PATH = vault dir) | jsonl (generic
           {subject,relation,object}|{content} lines) | tool-log (OpenAI-style
           tool-call JSONL → Tool grains, feeds tool-failure clustering).
           Re-runs skip what is already imported; see docs/migrate.md.
  reindex                             backfill + rebuild the BM25 text index
                                      (e.g. after --index-text true on a file
                                      written with indexing off)
  stream   --to DIR [--interval-ms N] [--once] [--checkpoint] [--retain 30d]
                                      continuous op-log shipping; --checkpoint
                                      opens a new generation with a full
                                      snapshot, --retain drops generations
                                      older than the window (so erasure
                                      reaches archives — see docs/gdpr.md)
  restore  --from DIR [--until-hlc T]            rebuild from stream dir (PITR)
  follow   --from DIR [--interval-ms N] [--once]  subscribe: apply new segments
                                                  (org/category distribution)
  related  --start TERM --relations R1,R2 [--direction out|in|both]
           [--depth N] [--limit N]    walk the entity graph (bounded k-hop).
                                      in/both only see relations the file
                                      declares entity-valued
  entity-at --subject S --relation R --at MS [--axis world|knowledge]
                                      as-of read: what was true at T (world)
                                      or what was known at T (knowledge)
  step-actions --workflow HASH [--node ID] [--limit N]
                                      execution records for a workflow —
                                      which grains ran which of its nodes
  eval     <create|run>                the §7.4 gating edge: create stores an
           evalset (--name N --cases FILE); run --evalset HASH executes it
           against --tool-cmd CMD or --model provider:name ([--base-url URL]
           [--key-env VAR] [--llm-max-tokens N] — how a tuned adapter served
           by vLLM/SGLang/Ollama is graded), journals each case under an
           eval- run id, and records the summary `areev loop apply
           --gating-run` loads
  tool     provenance <hash> [--depth N]   one-command code forensics: the
           recommendations targeting this code, each transition's approver +
           BECAUSE + gating edge, and the runs that touched it
  run      <start|resume|respond|cancel|list|inspect|verify|fork|shadow|
           oversight-report|demo>   the governed
           workflow runtime: journaled, checkpointed, HITL-pausable runs of
           Workflow grains. start --workflow HASH --run-id ID [--input JSON]
           [--tool-cmd CMD] [--model provider:name] [--base-url URL]
           [--key-env VAR] [--llm-max-tokens N]
           [--events] [--as PRINCIPAL] [--max-tokens N --max-usd F ...]
           [--allow-executor ADDR,...] [--executor-cache DIR]
           [--sandbox-cmd 'areev-sandbox'] [--executor-timeout SECS]
           [--credential NAME=ENV_VAR[@PRINCIPAL],...] [--allow-host URL,...]
           [--tool-egress TOOL:CRED[@HOST]+...:METHOD+METHOD,...]
           [--credential-ttl SECS] [--resolver-env VAR,...];
           --credential/--allow-host/--tool-egress broker a tool's outbound
           calls: it gets the broker's address and a capability token, never
           the secret. A tool with no grant gets nothing, and a grant naming
           no method may only read. NAME=ENV_VAR@PRINCIPAL binds the
           credential to a run principal, so a run executing as anyone else is
           refused it — for a process holding several principals' secrets.
           CRED@HOST pairs a credential with the bare hostname it may be sent
           to, so a tool holding two services' secrets cannot send one to the
           other. A credential can also be MINTED per call instead of read
           from the environment: NAME=cmd:COMMAND takes the command's stdout
           (`cmd:gcloud auth print-access-token`, `cmd:vault kv get -field=t
           secret/x`) and NAME=vault:PATH#FIELD reads Vault/OpenBao directly
           over $VAULT_ADDR/$VAULT_TOKEN — both cached for --credential-ttl
           (default 300s), re-minted after it, and refused rather than sent
           unauthenticated if the resolver fails. Bind a principal to a minted
           credential on the NAME side (NAME@PRINCIPAL=cmd:...), because a
           command may itself contain '@'. --resolver-env names the variables
           a resolver needs for its OWN authentication ($VAULT_TOKEN,
           $AWS_PROFILE): they are withheld from every other subprocess and
           re-admitted only for resolvers;
           --allow-executor pins the content address of a code-carrying tool
           (a Definition whose executor_uri names a cas:// blob). Nothing
           code-carrying runs unpinned, because the blob travels with the
           memory and the authorization must not;
           --executor-timeout overrides the fixed 300s wall-clock ceiling a
           host-executed tool (--tool-cmd or a pinned --allow-executor blob)
           otherwise runs under — a document-analysis leg making a dozen
           model calls needs longer than that default was sized for. 0 means
           wait forever;
           fork --run-id BASE --as-run NEW [--at N] [--plan HASH]
           time-travels or migrates a run. `areev run demo` seeds the
           10-minute proof
  run-trace --run-id ID [--limit N]   what a run recorded, and what it
                                      produced downstream (facts/lessons)
  runs-touching --hash H [--depth N]  which runs produced or refined a grain
                                      (walks provenance both ways)
  verify                              integrity + content-address recheck
  stats                               store counters
  serve    --mcp [--ns NS] [--mount alias=path,...] [--no-destructive-ops] [--lock-ns NS] [--profile memory|full]  MCP server on stdio
                                      (--mount adds read-only files for
                                       cross-file ASSEMBLE; ns \"alias.inner\";
                                       --profile memory drops the run/loop
                                       tool family, default full)
  repl     [--ns NS]                  interactive CAL console in the terminal
  remember --content TEXT [--facts JSON] [--observer ID]
           [--session-id ID] [--run-id ID] [--role user|assistant|system|tool]
           [--model SPEC | --llm-cmd CMD] [--extract-hint TEXT]
           [--ground-model SPEC | --ground-cmd CMD]
           [--min-confidence F] [--dry-run]
                                      store free text as an Event, then
                                      attach the facts distilled from it.
                                      --facts is host-supplied and skips the
                                      model; --model/--llm-cmd extract with an
                                      LLM (stamped verification_status
                                      \"unverified\" unless --ground-* checks
                                      them). --dry-run prints, stores nothing.
  hook     claude-code               print settings.json hook snippet
                                      (auto recall-before-prompt + capture-on-stop)
  anonymize scan [--text T] [--policy-file F]
                                      detect sensitive spans (Tier-0 chain:
                                      structural, regex+checksum, secrets,
                                      keyword cues, dictionary). Reads stdin
                                      when --text is absent; prints JSON.
                                      Pure text — needs no memory file.
  anonymize test --fixtures F [--policy-file F]
                                      Assert a policy against a fixture file
                                      of must-redact / must-not-redact strings.
                                      Exits non-zero on any miss or false
                                      positive — run it in CI.
  anonymize set --ns NS (--policy-file F | --policy JSON)
                                      declare a per-namespace egress/audit
                                      policy (travels with the file; stamps
                                      min_reader_version). `list` shows the
                                      declared policies, `clear --ns NS`
                                      removes one, `mappings` prints this
                                      process's live pseudonym mappings, and
                                      `reveal --ns NS --token T` reverse-looks
                                      one up (admin verb; Tier-2 audited by
                                      fingerprint). Add --anonymize-egress to
                                      any verb to force egress on as a host
                                      floor, and --anonymize-cmd 'CMD' to
                                      install a Tier-1 NER detector (JSON
                                      probe/detect over stdin/stdout).
  memtool  '<COMMAND-JSON>'           Anthropic memory-tool ops on grains
  auth     mint|list|revoke --auth FILE
                                      manage the credential map (no --db: it
                                      names no memory). mint --id NAME
                                      --principal NAME [--label TEXT]
                                      [--expires 90d|<ISO-8601>]
                                      [--memories A,B] prints a 256-bit
                                      areev_pat_ token ONCE on stdout and
                                      stores only its SHA-256; revoke --id
                                      NAME drops one credential without
                                      touching the principal's others
                                      (restart `areev ui` to apply)
  ui       [--addr HOST:PORT] [--allow-remote] [--allow-origin ORIGIN[,ORIGIN...]]
           [--token-env VAR] [--no-destructive-ops] [--tls-cert PATH --tls-key PATH]
           [--sso-header NAME --sso-secret-env VAR [--sso-secret-env-next VAR]]
           [--sso-approvals deny|allow] [--sso-groups-header NAME]
           [--sso-principal-prefix PREFIX]
           [--oidc-issuer URL --oidc-client-id ID --oidc-client-secret-env VAR
            --oidc-redirect-uri URL [--oidc-scopes S] [--oidc-principal-claim C]]
                                      web console (default 127.0.0.1:7437).
                                      --allow-origin accepts a cross-origin
                                      POST from an EXACT origin (comma-
                                      separated for more than one; no
                                      wildcards) — the Origin check is not
                                      lifted by --allow-remote, so a console
                                      behind a reverse proxy needs its public
                                      origin named here to be usable.
                                      --sso-secret-env-next opens a rotation
                                      window: both secrets prove the proxy, so
                                      the fleet moves over one node at a time
                                      (docs/runbooks/sso-secret-rotation.md)
                                      --sso-approvals (default deny) decides
                                      whether a proxy-asserted identity may
                                      answer a HITL approval: its only proof
                                      is the shared proxy secret, so 'allow'
                                      makes every approval only as strong as
                                      that one value. Approvers should hold a
                                      per-principal credential (--auth).
                                      --sso-groups-header maps IdP groups to
                                      principals via --auth's \"groups\" table
                                      (needs --auth); an identity with its own
                                      grants outranks its groups, and a
                                      group-derived principal is a ROLE — it
                                      may never approve, under any setting.
                                      --sso-principal-prefix stamps every
                                      proxy-asserted principal so IdP names
                                      stay distinct from local ones.
                                      --oidc-* (build with --features oidc)
                                      runs the auth-code+PKCE flow in-process
                                      instead: the console verifies the IdP's
                                      signature against its published key set
                                      and issues an HttpOnly SameSite=Strict
                                      session cookie. Login at /auth/login,
                                      logout at /auth/logout. An OIDC identity
                                      MAY approve (no --sso-approvals needed):
                                      a verified signature is a stronger claim
                                      than a shared proxy secret. The redirect
                                      URI must match the provider's exactly.

Namespace defaults to \"shared\". Exit code 0 on success.
--db is optional for one-shot commands: it falls back to $AREEV_DB, then
~/.areev/default.db. `serve` and `ui` require an explicit --db (or $AREEV_DB).
Files carry their own settings (text index, entity relations, embedding
provenance) in an internal meta table; a bare open honors them.
--index-text true|false explicitly re-stamps the file's declaration.
--assembly-manifest-sample-rate 0..1 samples ASSEMBLE provenance into
agent:harness (host-only, default 0/off).

Principals: add --as <principal> to cal/repl/serve/subject-report/forget-subject/
purge-older-than to run the session under the file's grants for that
principal (fail closed; see GRANT in docs/cal-reference.md §9). --auth FILE
validates the principal against a credential map, and `areev ui --auth FILE`
serves the console in multi-principal mode (tokens → principals,
unauthenticated = read-only \"anonymous\").

Console auth: prefer --auth <map> — per-principal credentials, so a write or
an approval names WHO did it, one credential can be revoked without disturbing
the principal's others, and `expires_at` bounds a leak. `areev auth mint`
creates them. --token-env <VAR> remains the single-user shortcut: one shared
secret, implied admin, unattributable (it can never answer a HITL approval).
On that path the token's entropy is the ONLY control — use at least 32 hex
characters, or let `areev auth mint` choose; comparison against the presented
credential is constant-time. After 10 consecutive failed authentications from
one source address, further credential-bearing requests from it are refused
with 429 until the streak goes idle (15 min). Browsers authenticate via HTTP Basic, which the
browser caches per origin and RE-ATTACHES AUTOMATICALLY, including on
cross-site requests — so the Origin check (--allow-origin, above) is
load-bearing, not defence in depth. There is no logout: closing the tab does
not clear a cached Basic credential, so a browser used to demo the console
stays authenticated until it is restarted or its saved credentials are
cleared. A wrong token is counted per source IP and logged to stderr as
\"console auth FAILED from <ip> (<n> consecutive)\" (greppable for a
fail2ban-style rule; the token itself is never logged) — but it is NOT
delayed or locked out in-process: `ui` serves one connection at a time, so a
sleep before responding would let an unauthenticated caller stall the
console for everyone, and behind a reverse proxy every request logs the
PROXY's IP anyway, so an IP lockout here would lock out every user at once.
Rate limiting belongs at that proxy (the documented default deployment
shape) — write the fail2ban-style rule against its access log, not this
one. A SameSite/HttpOnly/Secure session cookie is the intended future fix
for the caching/logout problem above and is tracked separately — it does
not exist yet.

Encryption at rest: add --passphrase-env <VAR> to any command to derive an
AES-256 key (Argon2id) from the passphrase in environment variable VAR. The
non-secret salt is kept in a <memory.db>.kdf sidecar — back it up with the db.

Anonymization key: add --anon-key-env <VAR> to any command to supply the
32-byte root (64 hex characters in VAR) the pseudonym session/memory/vault
subkeys are derived from. Independent of the page cipher, so the mapping vault
and value-derived tokens also work on postgres and on plaintext files. Never
persisted; rotating it is a crypto-erasure of the mapping table.

Vector recall: add --embed-cmd 'CMD' [--embed-model NAME] to any command to
install a command embedder — CMD gets the text on stdin and must print a JSON
array of numbers. Turns on the vector leg for search/serve, and embeds grains
written by add/remember/migrate.

Read-only: add --read-only to any command to open the memory refusing every
write (add/supersede/forget/reindex/blob-put and so on fail with a coded
STO-E004 error); reads and CAL SELECT work normally. On postgres this also
skips schema/table bootstrap, so a role holding only CONNECT + USAGE + SELECT
grants can open an existing, already-migrated memory — see
docs/deployment-profile.md for the grant recipe. --read-only combined with an
explicit --index-text is refused up front (--index-text always re-stamps the
file's declaration, which read-only mode cannot do).";

fn flag(args: &HashMap<String, String>, k: &str) -> Option<String> {
    args.get(k).cloned()
}

fn assembly_manifest_sample_rate(flags: &HashMap<String, String>) -> Result<f64, String> {
    let Some(raw) = flag(flags, "assembly-manifest-sample-rate") else {
        return Ok(0.0);
    };
    let rate: f64 = raw
        .parse()
        .map_err(|_| "--assembly-manifest-sample-rate must be a number from 0 to 1".to_string())?;
    if !rate.is_finite() || !(0.0..=1.0).contains(&rate) {
        return Err("--assembly-manifest-sample-rate must be from 0 to 1".into());
    }
    Ok(rate)
}

/// Map a short flag to its long name. Terse aliases for what you type on every
/// invocation: `-d` for `--db`, `-k` for the recall/search limit.
fn short_flag(a: &str) -> Option<&'static str> {
    match a {
        "-d" => Some("db"),
        "-k" => Some("k"),
        _ => None,
    }
}

fn need(args: &HashMap<String, String>, k: &str) -> Result<String, String> {
    flag(args, k).ok_or_else(|| format!("missing required --{k}"))
}

/// Resolve the memory file: `--db`/`-d`, else `$AREEV_DB`, else the default
/// personal memory `~/.areev/default.db` (its parent dir is created, and a
/// one-line note prints to stderr so the file stays discoverable).
///
/// When `require_explicit` is set, the default fallback is refused with an
/// error instead. Long-lived, potentially network-exposed commands (`serve`,
/// `ui`) pass this: silently serving the personal default memory — over MCP or
/// an unauthenticated HTTP console — risks exposing or mutating the wrong file,
/// so those commands must name their memory explicitly.
/// A host-supplied 32-byte anonymization root, as 64 hex characters.
///
/// Wrong length or a stray non-hex character has to fail here, loudly. The key
/// derives the token space, so a silently different key looks like working
/// software right up until a rehydrate comes back empty — and by then the
/// tokens it wrote are already in the file.
fn parse_anon_key(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err(format!(
            "expected 64 hex characters (a 32-byte key), got {}",
            hex.len()
        ));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16)
            .map_err(|_| format!("not hex: {pair:?} at byte {i}"))?;
    }
    Ok(out)
}

/// Open a memory on the postgres backend from a `postgres://…?schema=<name>`
/// DSN, mirroring the file-backend branches (explicit options re-stamp;
/// telemetry rides the memory's schema). Encryption keys don't apply here —
/// the `--passphrase-env` path derives them from a `.kdf` sidecar next to a
/// FILE, so a DSN is rejected before derivation.
///
/// Transport security is the DSN's business, not a flag's: `sslmode=` and
/// `sslrootcert=` ride through `split_schema_url` untouched and are honored by
/// the store (`feature = "postgres-tls"`; without it an encrypting DSN is
/// refused with `STO-E003` rather than downgraded). See
/// `docs/deployment-profile.md` §"Postgres connection contract".
#[cfg(feature = "postgres")]
fn open_postgres_store(
    db: &str,
    tel_mode: areev_store::TelemetryMode,
    explicit_index: Option<&str>,
    anon_key: Option<[u8; 32]>,
    read_only: bool,
) -> Result<Areev, String> {
    let (url, schema) = areev_store::pg::split_schema_url(db).map_err(|e| e.to_string())?;
    // An --anon-key-env key is the whole reason the mapping vault and
    // value-derived tokens are reachable on this backend at all: Postgres
    // refuses `encryption_key` (a page-cipher capability), so without a
    // host-supplied root there is no key material to derive them from.
    // --read-only also forces the explicit-options path: it needs an
    // `AreevOptions` to carry even when nothing else does, so the least-
    // privilege open (skip bootstrap, verify instead) actually happens.
    if explicit_index.is_some() || anon_key.is_some() || read_only {
        let o = areev_store::AreevOptions {
            index_text: explicit_index
                .map(|v| !matches!(v, "false" | "0" | "off" | "no"))
                .unwrap_or(areev_store::AreevOptions::default().index_text),
            anon_key,
            telemetry: tel_mode,
            read_only,
            ..Default::default()
        };
        Areev::open_postgres_with(&url, &schema, o)
    } else if tel_mode != areev_store::TelemetryMode::Off {
        Areev::open_postgres_with_telemetry(&url, &schema, tel_mode)
    } else {
        Areev::open_postgres(&url, &schema)
    }
    .map_err(|e| e.to_string())
}

#[cfg(not(feature = "postgres"))]
fn open_postgres_store(
    _db: &str,
    _tel_mode: areev_store::TelemetryMode,
    _explicit_index: Option<&str>,
    _anon_key: Option<[u8; 32]>,
    _read_only: bool,
) -> Result<Areev, String> {
    // Name `postgres-tls`, not `postgres`: the bare `postgres` feature
    // REFUSES any DSN carrying `sslmode=` (that gate is `postgres-tls`'s
    // job), and every managed Postgres that requires TLS on the wire —
    // Azure Flexible Server, RDS with `rds.force_ssl`, Cloud SQL — needs
    // exactly that. `cargo install areev` itself is real (crates.io has it);
    // only the feature name was wrong.
    Err("this build lacks the postgres backend — reinstall with \
         `cargo install areev --features postgres-tls` (or build with \
         --features postgres-tls), or grab the `-postgres` asset from a \
         GitHub release, which ships the backend already compiled in"
        .into())
}

fn resolve_db(args: &HashMap<String, String>, require_explicit: bool) -> Result<String, String> {
    if let Some(p) = flag(args, "db") {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("AREEV_DB") {
        if !p.trim().is_empty() {
            return Ok(p);
        }
    }
    if require_explicit {
        return Err(
            "this command needs an explicit memory file — pass --db <file> (or -d), \
             or set $AREEV_DB. It will not fall back to the personal default memory \
             (~/.areev/default.db), to avoid serving the wrong file."
                .to_string(),
        );
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "no --db given, and neither $AREEV_DB nor $HOME is set".to_string())?;
    let path = format!("{home}/.areev/default.db");
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    eprintln!("areev: using default memory {path} (override with -d/--db or $AREEV_DB)");
    Ok(path)
}

/// Bind the session principal from `--as` (validated against `--auth`'s
/// credential map when one is given): rights become exactly what the file's
/// grant grains cover for that principal — fail closed. Absent `--as`, the
/// local session stays the owner.
fn apply_principal(
    facade: areev_cal::AreevFacade,
    flags: &std::collections::HashMap<String, String>,
) -> Result<areev_cal::AreevFacade, String> {
    let Some(p) = flag(flags, "as") else {
        return Ok(facade);
    };
    if let Some(path) = flag(flags, "auth") {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("--auth {path}: {e}"))?;
        let map = areev_core::authz::CredentialMap::from_json(&text).map_err(|e| e.to_string())?;
        map.knows_principal(&p).map_err(|e| e.to_string())?;
    }
    let bound = facade.with_principal(&p).map_err(|e| e.to_string())?;
    eprintln!("areev: session bound to principal '{p}' (rights from the file's grants)");
    Ok(bound)
}

/// Record a Tier-2 audit Observation for a host-level erasure (GDPR
/// Art. 5(2)/30). The CLI is a host: REQ-ERASE-5 keeps the audit grain out
/// of the *engine* so an erasure never writes the identity back, and leaves
/// the record to whoever invoked it — which for `areev forget-subject` /
/// `purge-older-than` is this. Uses the same builder as the CAL path so
/// `areev audit export` reads one shape, and the principal is `--as` when
/// given (else the local owner session).
///
/// Best-effort by construction: the erasure has already committed and
/// replicated. A failure to append the audit grain is reported loudly on
/// stderr but must not turn an accomplished erasure into an error exit —
/// that would tell the operator the deletion failed when it did not.
fn write_erasure_audit(
    m: &mut areev_store::Areev,
    flags: &HashMap<String, String>,
    verb: &str,
    target: &str,
    count: usize,
    stale_exports: &[areev_store::CorpusExportRegistry],
) {
    let principal = flag(flags, "as").unwrap_or_else(|| "local:owner".to_string());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let because = flag(flags, "because");
    let mut obs = areev_core::authz::audit_observation(
        &principal,
        verb,
        target,
        because.as_deref(),
        count,
        now,
    );
    if !stale_exports.is_empty() {
        let mut context = obs
            .common
            .context
            .take()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        context.insert(
            "stale_corpora".into(),
            serde_json::to_value(stale_exports).unwrap_or_else(|_| serde_json::json!([])),
        );
        // One provenance hop further: adapters trained on a stale corpus
        // are stale too (`areev tune` writes `derived_from` = the export
        // manifest). The erasure cannot reach into a checkpoint; it CAN say
        // precisely which adapters must be retired or re-derived — that
        // report is the whole point of the lineage.
        let mut stale_adapters = Vec::new();
        for export in stale_exports {
            let Ok(h) = Hash::from_hex(&export.manifest_hash) else {
                continue;
            };
            let kids = m.grains_derived_from(&h).unwrap_or_default();
            for g in kids {
                if g.get_str("relation") != Some("mg:adapter") {
                    continue;
                }
                stale_adapters.push(serde_json::json!({
                    "subject": g.get_str("subject"),
                    "grain_hash": g.hash.to_hex(),
                    "export_id": export.export_id,
                }));
            }
        }
        if !stale_adapters.is_empty() {
            context.insert("stale_adapters".into(), serde_json::json!(stale_adapters));
        }
        obs.common.context = Some(serde_json::Value::Object(context));
        for export in stale_exports {
            let recipient = export
                .recipient
                .as_deref()
                .map(|id| format!(" for recipient {id}"))
                .unwrap_or_default();
            eprintln!(
                "areev: corpus {} at {}{} is stale and must be retired or re-derived",
                export.export_id, export.destination, recipient
            );
        }
        for a in &stale_adapters {
            eprintln!(
                "areev: adapter {} (grain {}) derives from stale corpus {} — retire or re-derive",
                a["subject"].as_str().unwrap_or("?"),
                a["grain_hash"].as_str().unwrap_or("?"),
                a["export_id"].as_str().unwrap_or("?"),
            );
        }
    }
    if let Err(e) = m.add(&obs) {
        eprintln!("areev: warning: erasure succeeded but its audit record failed to write: {e}");
    }
}

/// True when a `host:port` (or bare host) names a loopback interface. Used to
/// refuse binding the unauthenticated `ui` console to a routable address.
/// Print any saved query or template the file carries that this build cannot
/// load, the same way `open_warnings` is printed.
///
/// Forces the registries to rehydrate first — they load lazily, so a verb that
/// never touches one would otherwise report nothing and the operator would find
/// out by noticing a saved query had gone missing.
fn report_meta_warnings(facade: &areev_cal::AreevFacade) {
    use areev_cal::CalStoreFacade;
    let _ = facade.list_queries();
    let _ = facade.list_templates();
    for w in facade.meta_warnings() {
        eprintln!("areev: warning: {w}");
    }
}

fn addr_is_loopback(addr: &str) -> bool {
    let a = addr.trim();
    // IPv6 bracketed form: [::1]:port
    if let Some(rest) = a.strip_prefix('[') {
        let host = rest.split(']').next().unwrap_or("");
        return host == "::1" || host.eq_ignore_ascii_case("localhost");
    }
    let host = a.rsplit_once(':').map(|(h, _)| h).unwrap_or(a);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // An unqualified hostname or 0.0.0.0 is not provably loopback → treat as remote.
        Err(_) => false,
    }
}

fn parse_args(rest: &[String]) -> (HashMap<String, String>, Vec<String>) {
    let mut flags = HashMap::new();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if let Some(name) = a.strip_prefix("--") {
            // A value is the next token only if it is not itself a flag. Reject
            // any leading '-' (not just "--"), so a valueless long flag doesn't
            // swallow a following short flag as its value — e.g. `serve --mcp -d
            // db` must parse `-d` as --db, not as the value of `--mcp`. This
            // matches the short-flag branch below. Flag values in this CLI never
            // start with '-'.
            if i + 1 < rest.len() && !rest[i + 1].starts_with('-') {
                flags.insert(name.to_string(), rest[i + 1].clone());
                i += 2;
            } else {
                flags.insert(name.to_string(), "true".to_string());
                i += 1;
            }
        } else if let Some(long) = short_flag(a) {
            if i + 1 < rest.len() && !rest[i + 1].starts_with('-') {
                flags.insert(long.to_string(), rest[i + 1].clone());
                i += 2;
            } else {
                flags.insert(long.to_string(), "true".to_string());
                i += 1;
            }
        } else {
            positional.push(a.clone());
            i += 1;
        }
    }
    (flags, positional)
}

/// Map one Claude Code transcript message onto OMS Event content blocks.
/// Tool-result bodies are bounded so a single command cannot exceed the grain
/// size cap; every elision is explicit and records the original byte length.
fn transcript_content_blocks(
    content: &serde_json::Value,
    turn: usize,
) -> (String, Vec<ContentBlock>, Vec<serde_json::Value>) {
    const TOOL_RESULT_CHARS: usize = 32 * 1024;

    fn truncate(s: &str, max: usize) -> (String, bool) {
        if s.chars().count() <= max {
            (s.to_string(), false)
        } else {
            (s.chars().take(max).collect(), true)
        }
    }
    fn tool_result_body(content: &serde_json::Value) -> String {
        match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str().or_else(|| b.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            other if other.is_object() => other["text"].as_str().unwrap_or("").to_string(),
            _ => String::new(),
        }
    }
    let input_blocks: Vec<&serde_json::Value> = match content {
        serde_json::Value::String(_) => Vec::new(),
        serde_json::Value::Array(blocks) => blocks.iter().collect(),
        _ => Vec::new(),
    };
    if let serde_json::Value::String(s) = content {
        return (
            s.clone(),
            vec![ContentBlock::Text { text: s.clone() }],
            Vec::new(),
        );
    }

    let mut rendered = Vec::new();
    let mut typed = Vec::new();
    let mut elisions = Vec::new();
    for (block_index, b) in input_blocks.into_iter().enumerate() {
        match b["type"].as_str() {
            Some("text") => {
                if let Some(text) = b["text"].as_str() {
                    rendered.push(text.to_string());
                    typed.push(ContentBlock::Text { text: text.to_string() });
                }
            }
            Some("tool_use") => {
                let name = b["name"].as_str().unwrap_or("tool").to_string();
                let id = b["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("synthetic:{turn}:{block_index}"));
                let input = b.get("input").cloned().unwrap_or(serde_json::Value::Null);
                rendered.push(format!("[tool_use {name}] {input}"));
                typed.push(ContentBlock::ToolUse { id, name, input });
            }
            Some("tool_result") => {
                let original = tool_result_body(&b["content"]);
                let original_bytes = original.len();
                let (body, elided) = truncate(&original, TOOL_RESULT_CHARS);
                let is_error = b.get("is_error").and_then(serde_json::Value::as_bool);
                let flag = if is_error == Some(true) { " ERROR" } else { "" };
                rendered.push(format!("[tool_result{flag}] {body}"));
                typed.push(ContentBlock::ToolResult {
                    tool_use_id: b["tool_use_id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("synthetic:{turn}:{block_index}")),
                    content: body,
                    is_error,
                });
                if elided {
                    elisions.push(serde_json::json!({
                        "block_index": block_index,
                        "kind": "tool_result",
                        "original_bytes": original_bytes,
                        "kept_chars": TOOL_RESULT_CHARS,
                    }));
                }
            }
            // Bare `{text: ...}` blocks occur in older transcripts.
            _ => {
                if let Some(text) = b["text"].as_str() {
                    rendered.push(text.to_string());
                    typed.push(ContentBlock::Text { text: text.to_string() });
                }
            }
        }
    }
    (rendered.join("\n"), typed, elisions)
}

/// The turn's own timestamp in epoch milliseconds, if the transcript carries
/// one — epoch integers, numeric strings, or the ISO-8601 form Claude Code
/// writes.
///
/// `capture-stop` pins `created_at` from this so a turn's content address is
/// stable across hook firings. Without a stable stamp the cumulative transcript
/// re-hashes every turn on every firing, and the dedup that makes the capture
/// idempotent stops working.
fn transcript_turn_ms(record: &serde_json::Value) -> Option<i64> {
    let value = record.get("timestamp").or_else(|| record.get("created_at"))?;
    if let Some(ms) = value.as_i64() {
        return Some(ms);
    }
    let text = value.as_str()?;
    if let Ok(ms) = text.parse::<i64>() {
        return Some(ms);
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn transcript_token_usage(message: &serde_json::Value) -> Option<TokenUsage> {
    let usage = message.get("usage")?.as_object()?;
    let u32_field = |primary: &str, alias: &str| {
        usage
            .get(primary)
            .or_else(|| usage.get(alias))
            .and_then(serde_json::Value::as_u64)
            .map(|n| n.min(u32::MAX as u64) as u32)
    };
    Some(TokenUsage {
        input_tokens: u32_field("input_tokens", "input").unwrap_or(0),
        output_tokens: u32_field("output_tokens", "output").unwrap_or(0),
        cache_read_tokens: u32_field("cache_read_tokens", "cache_read_input_tokens"),
        cache_creation_tokens: u32_field(
            "cache_creation_tokens",
            "cache_creation_input_tokens",
        ),
    })
}

/// Dispatch one `areev migrate --from <src>` run. Every source is file-based
/// (docs/migrate.md has the dump one-liners); `basic-memory` takes the vault
/// directory and walks its markdown files.
fn run_migrate(
    m: &mut Areev,
    ns: &str,
    from: &str,
    file: &str,
    history: Option<String>,
) -> Result<areev_store::migrate::MigrateReport, String> {
    use areev_store::migrate as mig;
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    let parse = |p: &str, s: &str| {
        serde_json::from_str::<serde_json::Value>(s).map_err(|e| format!("{p}: bad JSON: {e}"))
    };
    let rep = match from {
        "mem0" => {
            let export = parse(file, &read(file)?)?;
            let history = match &history {
                Some(h) => Some(parse(h, &read(h)?)?),
                None => None,
            };
            mig::migrate_mem0(m, ns, Some(&export), history.as_ref())
        }
        "mem0-history" => {
            let h = parse(file, &read(file)?)?;
            mig::migrate_mem0(m, ns, None, Some(&h))
        }
        "langgraph" | "langmem" => mig::migrate_langgraph(m, ns, &read(file)?),
        "letta" => {
            let af = parse(file, &read(file)?)?;
            mig::migrate_letta(m, ns, &af)
        }
        "letta-archival" => mig::migrate_letta_archival(m, ns, &read(file)?),
        "zep" | "graphiti" => {
            let v = parse(file, &read(file)?)?;
            mig::migrate_zep(m, ns, &v)
        }
        "jsonl" => mig::migrate_jsonl(m, ns, &read(file)?),
        // Generic tool-call log (OpenAI-style JSONL) → Tool grains, so the
        // flagship analyzer can cluster failures from history that predates
        // Areev. One line per record; assistant `tool_calls` arrays expand.
        "tool-log" | "openai-tools" => {
            let mut rep = mig::MigrateReport::default();
            for (i, line) in read(file)?.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_str(line)
                    .map_err(|e| format!("{file}:{}: bad JSON: {e}", i + 1))?;
                for (record_index, record) in extract_tool_records(&v).into_iter().enumerate() {
                    use sha2::{Digest, Sha256};
                    let synthetic_call_id = || {
                        let mut digest = Sha256::new();
                        digest.update(file.as_bytes());
                        digest.update([0]);
                        digest.update(line.as_bytes());
                        digest.update((i as u64).to_le_bytes());
                        digest.update((record_index as u64).to_le_bytes());
                        format!("import:{}", &hex::encode(digest.finalize())[..24])
                    };
                    let mut tool = Tool::new(&record.name)
                        .is_error(record.is_error)
                        // Unknown source time is pinned, not replaced with the
                        // import wall clock, so rerunning the same import can
                        // actually deduplicate it.
                        .created_at(record.created_at.unwrap_or(0));
                    if let Some(input) = record.input {
                        tool = tool.input(input);
                    }
                    if !record.content.is_empty() {
                        tool = tool.content(&record.content);
                    }
                    // The per-record digest is stable across reruns but
                    // distinguishes identical retries on separate lines.
                    let call_id = record.call_id.unwrap_or_else(synthetic_call_id);
                    tool = tool.tool_call_id(&call_id);
                    let tool = tool.namespace(ns);
                    let (_, hash) = areev_core::format::serialize::serialize_grain(&tool)
                        .map_err(|e| e.to_string())?;
                    if m.get(&hash).is_ok() {
                        rep.skipped += 1;
                    } else {
                        m.add(&tool).map_err(|e| e.to_string())?;
                        rep.added += 1;
                    }
                }
            }
            Ok(rep)
        }
        "basic-memory" => {
            let root = std::path::PathBuf::from(file);
            if !root.is_dir() {
                return Err(format!(
                    "--from basic-memory expects --file to be the vault directory (got {file})"
                ));
            }
            // Deterministic walk: collect *.md paths, sorted.
            let mut notes: Vec<std::path::PathBuf> = Vec::new();
            let mut stack = vec![root.clone()];
            while let Some(d) = stack.pop() {
                let entries = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
                for entry in entries {
                    let path = entry.map_err(|e| e.to_string())?.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|x| x == "md") {
                        notes.push(path);
                    }
                }
            }
            notes.sort();
            let mut rep = mig::MigrateReport::default();
            for path in notes {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let md = read(&path.to_string_lossy())?;
                let mtime_ms = std::fs::metadata(&path)
                    .and_then(|md| md.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                mig::migrate_basic_memory_note(m, ns, &rel, &md, mtime_ms, &mut rep)
                    .map_err(|e| e.to_string())?;
            }
            Ok(rep)
        }
        other => {
            return Err(format!(
                "unknown --from '{other}' — sources: mem0, mem0-history, langgraph, letta, \
                 letta-archival, zep, basic-memory, jsonl, tool-log"
            ))
        }
    };
    rep.map_err(|e| e.to_string())
}

#[derive(Debug, Clone, PartialEq)]
struct ImportedToolRecord {
    name: String,
    input: Option<serde_json::Value>,
    content: String,
    is_error: bool,
    call_id: Option<String>,
    created_at: Option<i64>,
}

/// Extract typed tool records from one tool-log JSONL
/// line: a direct `{tool_name, content, is_error}` record, an OpenAI
/// `role:"tool"` result, or an assistant message carrying a `tool_calls` array.
fn extract_tool_records(v: &serde_json::Value) -> Vec<ImportedToolRecord> {
    let stringify = |x: &serde_json::Value| match x {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let created_at = v
        .get("created_at")
        .or_else(|| v.get("timestamp"))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|s| s.parse::<i64>().ok()))
        });
    // Assistant message with a tool_calls array: one record per call.
    if let Some(calls) = v.get("tool_calls").and_then(|c| c.as_array()) {
        return calls
            .iter()
            .filter_map(|c| {
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .or_else(|| c.get("name").and_then(|n| n.as_str()))?;
                let input = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .or_else(|| c.get("input"))
                    .map(|value| match value {
                        serde_json::Value::String(raw) => serde_json::from_str(raw)
                            .unwrap_or_else(|_| serde_json::Value::String(raw.clone())),
                        other => other.clone(),
                    });
                let is_error = c
                    .get("is_error")
                    .and_then(|e| e.as_bool())
                    .unwrap_or_else(|| c.get("error").is_some());
                let call_id = c
                    .get("id")
                    .or_else(|| c.get("tool_call_id"))
                    .and_then(|id| id.as_str())
                    .map(str::to_string);
                Some(ImportedToolRecord {
                    name: name.to_string(),
                    input,
                    content: String::new(),
                    is_error,
                    call_id,
                    created_at,
                })
            })
            .collect();
    }
    // A single tool record / tool-result message.
    let name = v
        .get("tool_name")
        .and_then(|n| n.as_str())
        .or_else(|| v.get("name").and_then(|n| n.as_str()))
        .or_else(|| v.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()));
    match name {
        Some(name) => {
            let content = v
                .get("content")
                .or_else(|| v.get("output"))
                .or_else(|| v.get("result"))
                .map(&stringify)
                .unwrap_or_default();
            let is_error = v
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or_else(|| v.get("error").is_some());
            let input = v.get("input").or_else(|| v.get("arguments")).map(|value| match value {
                serde_json::Value::String(raw) => serde_json::from_str(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.clone())),
                other => other.clone(),
            });
            let call_id = v
                .get("tool_call_id")
                .or_else(|| v.get("call_id"))
                .or_else(|| v.get("id"))
                .and_then(|id| id.as_str())
                .map(str::to_string);
            vec![ImportedToolRecord {
                name: name.to_string(),
                input,
                content,
                is_error,
                call_id,
                created_at,
            }]
        }
        None => Vec::new(),
    }
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match argv.first() {
        Some(c) => c.clone(),
        None => {
            println!("{USAGE}");
            return Ok(());
        }
    };
    if cmd == "help" || cmd == "--help" || cmd == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    if cmd == "version" || cmd == "--version" || cmd == "-V" {
        println!("areev {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (flags, positional) = parse_args(&argv[1..]);

    // Register the environment variables the operator has told us hold secrets,
    // before anything can spawn a child. Every subprocess seam scrubs these.
    // Registering by NAME is what makes this possible: `--passphrase-env VAR`
    // passes the variable name, not the value, so we know exactly what to
    // withhold without ever having to guess which ambient variables are
    // sensitive. Done here, at the one choke point every verb passes through,
    // rather than at each flag's read site — `--token-env` is consumed inside
    // two different verb arms, and a seam that spawned before the arm ran would
    // have leaked it.
    // Both registries: areev-loop owns the `--llm-cmd` and `--analyzer-cmd`
    // seams and cannot depend on an areev-* sibling, so it keeps its own.
    for var_flag in [
        "passphrase-env",
        "token-env",
        "anon-key-env",
        // [A0/A3] Both are impersonation-grade: the proxy secret can assert
        // any identity, and the OIDC client secret can redeem codes as the
        // console itself. Neither may reach a --tool-cmd subprocess.
        "sso-secret-env",
        "sso-secret-env-next",
        "oidc-client-secret-env",
    ] {
        if let Some(var) = flag(&flags, var_flag) {
            areev_core::proc::deny_env_var(&var);
            areev_loop::proc::deny_env_var(&var);
        }
    }
    // `--credential NAME=ENV_VAR` names a variable the same way, but spells it
    // on the right of an `=` — so it needs its own parse rather than a fourth
    // entry in the list above (#100). `Credential::bearer_from_env` registers
    // the core seam for every host that reads one; this adds the loop mirror,
    // and does it HERE rather than in `build_egress` because the choke point
    // runs before any verb arm can spawn.
    if let Some(list) = flag(&flags, "credential") {
        for pair in list.split(',') {
            if let Some((_, spec)) = pair.split_once('=') {
                let spec = spec.trim();
                // A `cmd:` spec names no variable to withhold — the secret is
                // minted at call time and never sits in the environment at
                // all, which is the point of #113. Skipping it also keeps a
                // shell command from being registered as a nonsense variable
                // name.
                if spec.starts_with("cmd:") {
                    continue;
                }
                // A `vault:` spec DOES imply variables: the broker reads
                // `$VAULT_TOKEN` (and friends) in-process to authenticate.
                // `CredentialSource::from_spec` registers them in the core
                // registry; the loop mirror is only reachable from here, so
                // both are done at this choke point like every other secret.
                if spec.starts_with("vault:") {
                    for var in ["VAULT_TOKEN", "VAULT_ADDR", "VAULT_NAMESPACE"] {
                        areev_core::proc::deny_env_var(var);
                        areev_loop::proc::deny_env_var(var);
                    }
                    continue;
                }
                // The spec may carry a `VAR@principal` owner (#101); the deny
                // list wants the bare variable name, so strip the qualifier.
                let var = spec.split_once('@').map(|(v, _)| v).unwrap_or(spec).trim();
                if !var.is_empty() {
                    areev_core::proc::deny_env_var(var);
                    areev_loop::proc::deny_env_var(var);
                }
            }
        }
    }
    // `--resolver-env` names the variables a credential RESOLVER needs for its
    // own authentication — `VAULT_TOKEN`, `AWS_PROFILE`, a service-account
    // path (#113). Registering them here is the load-bearing half: a vault
    // token is a secret one class more powerful than the credential it
    // fetches, since it can fetch all of them, and leaving it in the ambient
    // environment would put it inside every `--tool-cmd` subprocess — the #100
    // leak reopened one level up. Withheld from every child here, and
    // re-admitted only for resolver spawns (`CredentialSource::spawn_policy`).
    if let Some(list) = flag(&flags, "resolver-env") {
        for var in list.split(',').map(str::trim).filter(|v| !v.is_empty()) {
            areev_core::proc::deny_env_var(var);
            areev_loop::proc::deny_env_var(var);
        }
    }

    // `auth` manages the host-side credential map (`areev-auth.json`) and
    // never opens a memory. Dispatched BEFORE `resolve_db` on purpose: the
    // map holds no policy and names no file, so resolving a default memory
    // here would both be wasted work and print a line implying an
    // association between a credential and a database that does not exist.
    if cmd == "auth" {
        return run_auth(&flags, &positional);
    }

    // Long-lived / exposed surfaces must name their memory explicitly rather
    // than silently defaulting to the personal file.
    let db = resolve_db(&flags, matches!(cmd.as_str(), "serve" | "ui"))?;
    let ns = flag(&flags, "ns").unwrap_or_else(|| "shared".to_string());

    // print-only verbs never open the store (paths may be untilde-expanded)
    if cmd == "hook" {
        let target = positional.first().map(String::as_str).unwrap_or("claude-code");
        if target != "claude-code" {
            return Err(format!("unknown hook target '{target}'"));
        }
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "areev".into());
        println!(
            r#"Add to ~/.claude/settings.json (hooks section) to close the learning
loop automatically — inject relevant memory before each prompt, and capture
each exchange (with tool outcomes) when a turn ends:

{{
  "hooks": {{
    "UserPromptSubmit": [{{ "hooks": [{{
      "type": "command",
      "command": "{exe} recall-hook --db {db} --ns {ns} --with-loop"
    }}] }}],
    "Stop": [{{ "hooks": [{{
      "type": "command",
      "command": "{exe} capture-stop --db {db} --ns {ns}"
    }}] }}]
  }}
}}

recall-hook reads the prompt and prints matching memories to stdout, which
Claude Code injects as context — so retrieval no longer depends on the model
choosing to call a tool. For on-demand reads/writes by the model itself, also
register the MCP server:
  claude mcp add areev -- {exe} serve --mcp --db {db} --ns {ns}

Nothing was written — apply the snippet yourself (or rerun with your own paths)."#
        );
        return Ok(());
    }

    // `anonymize scan` is pure text processing: it never opens the store,
    // so it runs before the open like `hook`. The policy verbs
    // (set/list/clear/mappings) are store-backed and dispatch below.
    if cmd == "anonymize" && positional.first().map(String::as_str) == Some("scan") {
        let text = match flag(&flags, "text") {
            Some(t) => t,
            None => {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .map_err(|e| format!("reading stdin: {e}"))?;
                buf
            }
        };
        let policy = match flag(&flags, "policy-file") {
            Some(path) => {
                let json = std::fs::read_to_string(&path)
                    .map_err(|e| format!("--policy-file {path}: {e}"))?;
                areev_core::anon::AnonPolicy::from_json(&json).map_err(|e| e.to_string())?
            }
            None => areev_core::anon::AnonPolicy::default(),
        };
        let out = areev_core::anon::scan(&text, &policy, &[]).map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    // `anonymize test` is pure text processing too: a policy plus a fixture
    // file, no store. It exists because an untestable egress control is close
    // to an unusable one — an operator could not assert "this policy must
    // redact these strings and must not redact those" and have CI fail on a
    // regression, which is the first artifact an auditor asks to see.
    if cmd == "anonymize" && positional.first().map(String::as_str) == Some("test") {
        let path = flag(&flags, "fixtures")
            .ok_or("anonymize test requires --fixtures <FILE> (JSON)")?;
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("--fixtures {path}: {e}"))?;
        let fx: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("--fixtures {path}: {e}"))?;
        let must_redact = fixture_strings(&fx, "must_redact", &path)?;
        let must_not_redact = fixture_strings(&fx, "must_not_redact", &path)?;
        // A policy on the CLI overrides the one in the file, so one fixture
        // set can be run against a candidate policy before it is deployed.
        let policy = match flag(&flags, "policy-file") {
            Some(pp) => {
                let json = std::fs::read_to_string(&pp)
                    .map_err(|e| format!("--policy-file {pp}: {e}"))?;
                areev_core::anon::AnonPolicy::from_json(&json).map_err(|e| e.to_string())?
            }
            None => match fx.get("policy") {
                Some(v) => areev_core::anon::AnonPolicy::from_json(&v.to_string())
                    .map_err(|e| e.to_string())?,
                None => return Err(format!("{path} has no \"policy\" and no --policy-file given")),
            },
        };

        let mut misses: Vec<String> = Vec::new();
        let mut false_positives: Vec<String> = Vec::new();
        for want in &must_redact {
            let got = areev_core::anon::anonymize(want, &policy, &[], None)
                .map_err(|e| e.to_string())?;
            // "Redacted" means the text changed: the policy acted on it,
            // whether by pseudonym, mask or redaction.
            if got.text == *want {
                misses.push(want.clone());
            }
        }
        for want in &must_not_redact {
            let got = areev_core::anon::anonymize(want, &policy, &[], None)
                .map_err(|e| e.to_string())?;
            if got.text != *want {
                false_positives.push(format!("{want}  ->  {}", got.text));
            }
        }

        let total = must_redact.len() + must_not_redact.len();
        let failed = misses.len() + false_positives.len();
        for m in &misses {
            eprintln!("MISS (should have been redacted): {m}");
        }
        for f in &false_positives {
            eprintln!("FALSE POSITIVE (should have been left alone): {f}");
        }
        println!(
            "{} fixtures: {} passed, {} failed ({} missed, {} false positive)",
            total,
            total - failed,
            failed,
            misses.len(),
            false_positives.len()
        );
        if failed > 0 {
            // Non-zero exit is the point: this runs in CI.
            std::process::exit(1);
        }
        return Ok(());
    }

    // Optional encryption: when --passphrase-env <VAR> is given, derive an
    // AES-256 key from the passphrase held in that environment variable
    // (Argon2id; salt in a <db>.kdf sidecar). The passphrase and the derived
    // key are held in zeroizing buffers; note the storage engine keeps the key
    // resident in memory while the database is open.
    let is_pg_url = db.starts_with("postgres://") || db.starts_with("postgresql://");
    let enc_key = match flag(&flags, "passphrase-env") {
        Some(_) if is_pg_url => {
            return Err(
                "--passphrase-env applies to file-backed memories (page cipher + .kdf \
                 sidecar); on the postgres backend use TDE/pgcrypto at the deployment layer"
                    .into(),
            );
        }
        Some(var) => {
            let pass = zeroize::Zeroizing::new(std::env::var(&var).map_err(|_| {
                format!("--passphrase-env {var}: environment variable is not set")
            })?);
            if pass.trim().is_empty() {
                return Err(format!("--passphrase-env {var}: passphrase is empty"));
            }
            Some(Areev::derive_key_for(&db, pass.as_str()).map_err(|e| e.to_string())?)
        }
        None => None,
    };

    // The anonymization root (--anon-key-env VAR, 64 hex characters in that
    // variable). Independent of the page cipher: it is the HKDF root for the
    // session/memory/vault subkeys when supplied, which is what makes the
    // mapping vault and deterministic value-derived tokens work on Postgres
    // (no page cipher there) and on plaintext files. Named by variable, never
    // taken on the command line, so it stays out of shell history and `ps`.
    // Never persisted — rotating it is a crypto-erasure of the mapping table,
    // so it belongs in a KMS.
    let anon_key = match flag(&flags, "anon-key-env") {
        Some(var) => {
            let raw = zeroize::Zeroizing::new(std::env::var(&var).map_err(|_| {
                format!("--anon-key-env {var}: environment variable is not set")
            })?);
            Some(parse_anon_key(raw.trim()).map_err(|e| format!("--anon-key-env {var}: {e}"))?)
        }
        None => None,
    };

    // `--read-only` computed up front (also used just below by `tel_mode`,
    // and again further down for the `--index-text` conflict check and the
    // open itself).
    let read_only = flags.contains_key("read-only");

    // Recall-telemetry sidecar (host capability, §8): the agent-host default is
    // `aggregate`; `--telemetry off|aggregate|full` overrides. It is NOT a
    // file-truth, so it never re-stamps the file's declarations. A read-only
    // handle attaches no sidecar regardless (its flush is a write) — when the
    // caller never asked for telemetry, resolve straight to `Off` so there is
    // nothing for the store to override (and nothing to warn about) on the
    // common `--read-only` path. An EXPLICIT `--telemetry aggregate|full`
    // together with `--read-only` is a real, deliberate conflict, so that
    // still flows through unchanged and the store's own override/warning
    // applies.
    let tel_mode = match flag(&flags, "telemetry") {
        Some(v) => areev_store::TelemetryMode::parse(&v)
            .ok_or_else(|| format!("--telemetry: unknown mode '{v}' (off|aggregate|full)"))?,
        None if read_only => areev_store::TelemetryMode::Off,
        None => areev_store::TelemetryMode::Aggregate,
    };

    // `areev blob get` is served BEFORE the open, from the `.blobs` sidecar.
    // The embedded backend's file lock is exclusive — while a run holds a
    // memory, a second process is refused even for a read — which would put an
    // attachment out of reach of the very `--tool-cmd` subprocess the run
    // spawned to process it. Blobs are immutable, live beside the file, and
    // carry their checksum as their address, so reading one needs neither the
    // database nor a lock. A sealed blob still needs the memory's derived key,
    // so that case falls through to the normal open below.
    if cmd == "blob" && positional.first().map(String::as_str) == Some("get") && !is_pg_url {
        let uri = positional
            .get(1)
            .ok_or("usage: areev blob get <cas-uri> --db <file>")?;
        if let Some(bytes) = areev_store::read_blob_offline(&db, uri).map_err(|e| e.to_string())? {
            use std::io::Write;
            std::io::stdout().write_all(&bytes).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }

    // Files carry their own declarations (meta table); a bare open honors
    // them. --index-text is an explicit, deliberate re-stamp; encryption is a
    // host-supplied capability that also requires open_with.
    let explicit_index = flag(&flags, "index-text");
    // --index-text always re-stamps the file's declaration (a write); a
    // read-only handle can never honor that, so refuse up front rather than
    // failing partway through open with a less legible error.
    if read_only && explicit_index.is_some() {
        return Err(
            "--read-only cannot be combined with an explicit --index-text: --index-text \
             always re-stamps the file's declaration, and a read-only open never writes. \
             Drop --index-text (a read-only open honors whatever the file already declares) \
             or drop --read-only"
                .into(),
        );
    }
    let mut m = if is_pg_url {
        open_postgres_store(&db, tel_mode, explicit_index.as_deref(), anon_key, read_only)?
    } else {
        if explicit_index.is_some() || enc_key.is_some() || anon_key.is_some() || read_only {
            let mut o = areev_store::AreevOptions::default();
            if let Some(v) = &explicit_index {
                o.index_text = !matches!(v.as_str(), "false" | "0" | "off" | "no");
            }
            if let Some(key) = &enc_key {
                o.encryption_key = Some(**key);
            }
            o.anon_key = anon_key;
            o.telemetry = tel_mode;
            o.read_only = read_only;
            Areev::open_with(&db, o)
        } else if tel_mode != areev_store::TelemetryMode::Off {
            // Honor the file's declarations AND attach the telemetry sidecar.
            Areev::open_with_telemetry(&db, tel_mode)
        } else {
            Areev::open(&db)
        }
        .map_err(|e| {
            let mut msg = e.to_string();
            // A .kdf sidecar means the file was created encrypted; opening it
            // without a key fails with an unhelpful "not a database" — say why.
            if enc_key.is_none() && std::path::Path::new(&format!("{db}.kdf")).exists() {
                msg.push_str(&format!(
                    " — {db}.kdf exists, so this memory is encrypted; pass --passphrase-env <VAR>"
                ));
            }
            msg
        })?
    };
    for w in m.open_warnings() {
        eprintln!("areev: warning: {w}");
    }
    // Host-scoped trajectory context for recall telemetry. The same flag is
    // also consumed by run-manifest/run-trace commands; it is not a file
    // declaration.
    m.set_run_id(flag(&flags, "run-id").as_deref());

    // Host cap (per-process, never persisted): --anonymize-egress forces
    // egress anonymization on every namespace without a declared policy and
    // upgrades a declared "off". It can never weaken a declared policy.
    if flags.contains_key("anonymize-egress") {
        m.set_anonymize_egress_floor(true);
    }
    // Tier-1 NER detector over the command seam (probed here, so a broken
    // command fails at startup instead of failing every covered read closed).
    if let Some(cmd_line) = flag(&flags, "anonymize-cmd") {
        let backend =
            areev_store::CommandAnonymize::new(&cmd_line).map_err(|e| e.to_string())?;
        m.set_anonymizer(Box::new(backend));
    }
    // Tier-2 LLM detector: a host command wrapping a (local-first) model.
    // Grounded — the model proposes span texts, never offsets, and anything
    // not present verbatim in the source is discarded.
    if let Some(cmd_line) = flag(&flags, "anonymize-llm-cmd") {
        let model = flag(&flags, "anonymize-llm-model");
        let llm = areev_loop::CommandLlm::new(&cmd_line, model.as_deref())
            .map_err(|e| e.to_string())?;
        m.set_anonymizer(Box::new(areev_llm::LlmDetector::new(llm)));
    }

    // Optional host-supplied embedder: --embed-cmd 'CMD' spawns CMD per embed
    // (text on stdin, JSON array of numbers on stdout). Enables the vector
    // recall leg and embeds newly written grains; the file records the model
    // as embedding provenance.
    if let Some(cmd_line) = flag(&flags, "embed-cmd") {
        let model = flag(&flags, "embed-model");
        let seen = m.open_warnings().len();
        let ce = areev_store::CommandEmbed::new(&cmd_line, model.as_deref())
            .map_err(|e| e.to_string())?;
        m.set_embedder(Box::new(ce));
        for w in &m.open_warnings()[seen..] {
            eprintln!("areev: warning: {w}");
        }
    }

    match cmd.as_str() {
        "add" => {
            // Positional `areev add <subject> <relation> <object>`, or the
            // explicit --subject/--relation/--object flags.
            let (s, r, o) = match (
                flag(&flags, "subject"),
                flag(&flags, "relation"),
                flag(&flags, "object"),
            ) {
                (Some(s), Some(r), Some(o)) => (s, r, o),
                _ if positional.len() >= 3 => (
                    positional[0].clone(),
                    positional[1].clone(),
                    positional[2].clone(),
                ),
                _ => {
                    return Err("usage: areev add <subject> <relation> <object>  \
                                (or --subject S --relation R --object O)"
                        .to_string())
                }
            };
            // An empty subject/relation/object stores an unrecallable fact —
            // reject at the door rather than pollute the memory file.
            for (name, v) in [("subject", &s), ("relation", &r), ("object", &o)] {
                if v.trim().is_empty() {
                    return Err(format!("{name} must not be empty"));
                }
            }
            let conf: f64 = flag(&flags, "confidence")
                .map(|c| c.parse().map_err(|_| "bad --confidence".to_string()))
                .transpose()?
                .unwrap_or(0.9);
            let mut f = Fact::new(&s, &r, &o).confidence(conf);
            f.common.namespace = Some(ns);
            // `--idempotent` collapses a re-add of the current value: if the
            // head for (subject, relation) already holds this object, the
            // existing grain's hash is returned instead of minting a new one.
            if flags.contains_key("idempotent") {
                let (h, inserted) = m.add_if_novel(&f).map_err(|e| e.to_string())?;
                println!("{h}");
                if !inserted {
                    eprintln!("(unchanged — value already current, no new grain)");
                }
            } else {
                let h = m.add(&f).map_err(|e| e.to_string())?;
                println!("{h}");
            }
        }
        "record-tool-call" => {
            let name = flag(&flags, "name")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| "usage: areev record-tool-call --name NAME --result TEXT [--input JSON] [--is-error] [--thread ID] [--call-id ID] [--run-id ID] [--workflow HASH --node ID] [--status pending|completed|failed] [--failure-cause CAUSE] [--executor-kind host|client] [--correlation-id ID]".to_string())?;
            let result = flag(&flags, "result")
                .or_else(|| positional.get(1).cloned())
                .ok_or_else(|| "record-tool-call requires --result TEXT".to_string())?;
            let input = flag(&flags, "input");
            let is_error = flag(&flags, "is-error")
                .map(|v| !matches!(v.as_str(), "false" | "0" | "off" | "no"))
                .unwrap_or(false);
            let facade = AreevFacade::with_session(m, Some(ns.clone()), None);
            let facade = apply_principal(facade, &flags)?;
            let hash = facade
                .record_tool_call(
                    &ns,
                    &name,
                    input.as_deref(),
                    &result,
                    is_error,
                    flag(&flags, "thread").as_deref(),
                    flag(&flags, "call-id").as_deref(),
                    flag(&flags, "run-id").as_deref(),
                    flag(&flags, "workflow").as_deref(),
                    flag(&flags, "node").as_deref(),
                    flag(&flags, "status").as_deref(),
                    flag(&flags, "failure-cause").as_deref(),
                    flag(&flags, "executor-kind").as_deref(),
                    flag(&flags, "correlation-id").as_deref(),
                )
                .map_err(|e| e.to_string())?;
            println!("{hash}");
        }
        "run-manifest" => {
            let run_id = flag(&flags, "run-id")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| "run-manifest requires --run-id ID".to_string())?;
            let config = flag(&flags, "config")
                .ok_or_else(|| "run-manifest requires --config JSON".to_string())?;
            let facade = AreevFacade::with_session(m, Some(ns), None);
            let facade = apply_principal(facade, &flags)?;
            let (config_hash, link_hash) = facade
                .record_run_manifest(&run_id, &config)
                .map_err(|e| e.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "run_id": run_id,
                    "config_hash": config_hash.to_hex(),
                    "link_hash": link_hash.to_hex(),
                })
            );
        }
        "recall" => {
            // Positional `areev recall <subject>`, or --subject S.
            let s = flag(&flags, "subject")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| "usage: areev recall <subject>  (or --subject S)".to_string())?;
            let rel = flag(&flags, "relation");
            let k: usize = flag(&flags, "k").and_then(|v| v.parse().ok()).unwrap_or(16);
            let grains = m
                .recall(&ns, &s, rel.as_deref(), k)
                .map_err(|e| e.to_string())?;
            match flag(&flags, "render") {
                None => {
                    for g in grains {
                        let line = serde_json::json!({
                            "hash": g.hash.to_hex(),
                            "type": format!("{:?}", g.grain_type).to_lowercase(),
                            "fields": g.fields,
                        });
                        println!("{line}");
                    }
                }
                Some(render) => {
                    // model-ready context via areev-context (§7.5)
                    use areev_context::{ContextAssembler, FormatPolicy, OutputFormat};
                    let mut policy = match render.as_str() {
                        "sml" => FormatPolicy::claude(),
                        "markdown" => FormatPolicy::gpt4(),
                        "toon" => FormatPolicy::new(OutputFormat::Toon),
                        "plain" => FormatPolicy::new(OutputFormat::PlainText),
                        "json" => FormatPolicy::json_api(),
                        other => return Err(format!("unknown --render '{other}'")),
                    };
                    if let Some(b) = flag(&flags, "budget").and_then(|v| v.parse().ok()) {
                        policy.token_budget = Some(b);
                    }
                    let hits: Vec<areev_cal::store_types::SearchHit> = grains
                        .into_iter()
                        .map(|grain| {
                            let hash = grain.hash;
                            areev_cal::store_types::SearchHit {
                                grain,
                                score: 1.0,
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
                        })
                        .collect();
                    let ctx = ContextAssembler::new().format(&hits, &policy);
                    println!("{}", ctx.text);
                    eprintln!(
                        "-- {} grains, ~{} tokens{}",
                        ctx.included_count,
                        ctx.estimated_tokens,
                        if ctx.truncated { " (truncated to budget)" } else { "" }
                    );
                }
            }
        }
        "search" => {
            let q = need(&flags, "query")?;
            let subject = flag(&flags, "subject");
            let k: usize = flag(&flags, "k").and_then(|v| v.parse().ok()).unwrap_or(10);
            let grains = m
                .recall_hybrid(&ns, subject.as_deref(), None, Some(&q), k, None)
                .map_err(|e| e.to_string())?;
            for g in grains {
                println!("{}", serde_json::json!({
                    "hash": g.hash.to_hex(),
                    "type": format!("{:?}", g.grain_type).to_lowercase(),
                    "fields": g.fields,
                }));
            }
        }
        "cal" => {
            let query = positional
                .first()
                .ok_or_else(|| "usage: areev cal '<QUERY>' --db <file>".to_string())?
                .clone();
            let facade = AreevFacade::with_session(m, Some(ns), None);
            report_meta_warnings(&facade);
            let facade = apply_principal(facade, &flags)?;
            let ex = CalExecutor::new(CalExecutorConfig {
                allow_destructive_ops: !flags.contains_key("no-destructive-ops"),
                assembly_manifest_sample_rate: assembly_manifest_sample_rate(&flags)?,
                ..CalExecutorConfig::default()
            })
            .with_governance(std::sync::Arc::new(areev_loop_adapter::LoopGovernance::new()));
            let res = ex.execute(&query, &facade).map_err(|e| e.to_string())?;
            let payload = serde_json::to_string_pretty(&res.result).map_err(|e| e.to_string())?;
            println!("{payload}");
            for w in res.warnings {
                eprintln!("warning: {w}");
            }
        }
        "corpus" => {
            let selector = flag(&flags, "select")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| {
                    "usage: areev corpus --select '<READ CAL>' [--out FILE] [--recipient ID]"
                        .to_string()
                })?;
            let facade = AreevFacade::with_session(m, Some(ns), None);
            report_meta_warnings(&facade);
            let facade = apply_principal(facade, &flags)?;
            let destination = flag(&flags, "out").unwrap_or_else(|| "stdout".to_string());
            let recipient = flag(&flags, "recipient");
            let (summary, manifest) =
                corpus::export(&facade, &selector, &destination, recipient.as_deref(), now_ms())?;
            eprintln!(
                "corpus export: {} row(s), {} source grain(s), manifest {}",
                summary.rows,
                summary.source_hashes.len(),
                manifest
            );
        }
        "tune" => {
            let facade = AreevFacade::with_session(m, Some(ns), None);
            report_meta_warnings(&facade);
            let facade = apply_principal(facade, &flags)?;
            return tune::run_tune(&facade, &flags);
        }
        "history" => {
            let s = need(&flags, "subject")?;
            let r = need(&flags, "relation")?;
            let versions = m.history(&ns, &s, &r).map_err(|e| e.to_string())?;
            for v in versions {
                println!(
                    "{}",
                    serde_json::json!({
                        "hash": v.hash.to_hex(),
                        "object": v.object,
                        "created_at": v.created_at,
                        "confidence": v.confidence,
                        "superseded_by": v.superseded_by.map(|h| h.to_hex()),
                    })
                );
            }
        }
        "log" => {
            let since: i64 = flag(&flags, "since").and_then(|v| v.parse().ok()).unwrap_or(0);
            let limit: usize = flag(&flags, "limit").and_then(|v| v.parse().ok()).unwrap_or(50);
            for op in m.changes_since(since, limit).map_err(|e| e.to_string())? {
                let kind = match op.op {
                    areev_store::OP_ADD => "add",
                    areev_store::OP_SUPERSEDE => "supersede",
                    areev_store::OP_FORGET => "forget",
                    _ => "?",
                };
                println!("{:>6}  {:>20}  {:<9}  {}", op.op_seq, op.hlc, kind, op.hash.to_hex());
            }
        }
        "stream" => {
            // Litestream-shaped: a full snapshot as segment 0 of each
            // generation, then incrementing delta segments. Cursor,
            // generation, and NEXT SEGMENT NUMBER live in the target dir,
            // so any machine holding the dir can restore.
            //
            // The segment number is tracked in CURSOR rather than derived
            // by counting directory entries: retention deletes old
            // generations, and a counted number would regress and
            // overwrite live segments.
            let to = need(&flags, "to")?;
            let interval: u64 = flag(&flags, "interval-ms").and_then(|v| v.parse().ok()).unwrap_or(500);
            let once = flags.contains_key("once");
            let checkpoint = flags.contains_key("checkpoint");
            let retain_ms = match flag(&flags, "retain") {
                Some(v) => Some(
                    parse_duration(&v)
                        .ok_or_else(|| format!("--retain takes a duration like 30d, got '{v}'"))?,
                ),
                None => None,
            };
            std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
            let cursor_path = format!("{to}/CURSOR");
            let (mut gen_id, mut cursor, mut seg) = read_stream_cursor(&cursor_path, &to);
            // A checkpoint (explicit, or the first run in an empty dir)
            // opens a NEW generation whose segment 0 is a full snapshot of
            // the live — already-erased — store. That is what lets
            // retention drop older generations without losing history:
            // everything still reachable is in this snapshot.
            if gen_id.is_empty() || checkpoint {
                let g = format!(
                    "{:016x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos() as u64
                );
                std::fs::create_dir_all(format!("{to}/gen-{g}")).map_err(|e| e.to_string())?;
                let snap = format!("{to}/gen-{g}/segment-{:08}.mgb", 0);
                let st = m.bundle_since(0, &snap).map_err(|e| e.to_string())?;
                gen_id = g;
                cursor = st.last_op_seq;
                seg = 1;
                std::fs::write(&cursor_path, format!("{gen_id} {cursor} {seg}"))
                    .map_err(|e| e.to_string())?;
                eprintln!("checkpoint: full snapshot of {} ops → {snap}", st.ops);
                if let Some(window) = retain_ms {
                    let dropped = prune_generations(&to, &gen_id, window)?;
                    if dropped > 0 {
                        eprintln!(
                            "retention: dropped {dropped} generation(s) older than the window \
                             (their contents are in this checkpoint, minus anything erased since)"
                        );
                    }
                }
            }
            loop {
                let ops_now = m.changes_since(cursor, 1).map_err(|e| e.to_string())?;
                if !ops_now.is_empty() {
                    let seg_path = format!("{to}/gen-{gen_id}/segment-{seg:08}.mgb");
                    let st = m.bundle_since(cursor, &seg_path).map_err(|e| e.to_string())?;
                    cursor = st.last_op_seq;
                    seg += 1;
                    std::fs::write(&cursor_path, format!("{gen_id} {cursor} {seg}"))
                        .map_err(|e| e.to_string())?;
                    eprintln!("shipped {} ops → {seg_path}", st.ops);
                }
                if once {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(interval));
            }
        }
        "follow" => {
            // Subscription pull (§5.10d): apply new segments from a stream
            // dir (local, NFS, or object-store mount). Follower keeps its
            // own cursor beside its db — offline edges catch up by replay.
            let from = need(&flags, "from")?;
            let interval: u64 = flag(&flags, "interval-ms").and_then(|v| v.parse().ok()).unwrap_or(1000);
            let once = flags.contains_key("once");
            // The follower cursor lives beside a FILE db; a DSN has no
            // "beside", so a postgres-backed follower keeps its cursor in
            // the stream dir, keyed by the memory's schema.
            #[cfg(feature = "postgres")]
            let fcur_path = if is_pg_url {
                let (_, schema) =
                    areev_store::pg::split_schema_url(&db).map_err(|e| e.to_string())?;
                format!("{from}/{schema}.follow")
            } else {
                format!("{db}.follow")
            };
            #[cfg(not(feature = "postgres"))]
            let fcur_path = format!("{db}.follow");
            loop {
                let cursor = std::fs::read_to_string(format!("{from}/CURSOR")).unwrap_or_default();
                let gen_id = cursor.trim().split(' ').next().unwrap_or("").to_string();
                if !gen_id.is_empty() {
                    let (fgen, fseg) = match std::fs::read_to_string(&fcur_path) {
                        Ok(s) => {
                            let mut it = s.trim().splitn(2, ' ');
                            (it.next().unwrap_or("").to_string(),
                             it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0))
                        }
                        Err(_) => (String::new(), 0),
                    };
                    // Re-baseline: the generation this follower was tracking
                    // may have aged out under `--retain`. Its segments are
                    // gone, but segment 0 of the CURRENT generation is a full
                    // snapshot — so restart from there rather than stalling
                    // forever on a missing file. Replay is idempotent, so
                    // re-importing already-held grains is a no-op.
                    if !fgen.is_empty()
                        && fgen != gen_id
                        && !std::path::Path::new(&format!("{from}/gen-{fgen}")).exists()
                    {
                        eprintln!(
                            "follow: generation {fgen} has aged out of the stream dir — \
                             re-baselining from generation {gen_id}'s snapshot"
                        );
                    }
                    let mut fseg = if fgen != gen_id { 0 } else { fseg };
                    loop {
                        let seg_path = format!("{from}/gen-{gen_id}/segment-{fseg:08}.mgb");
                        if !std::path::Path::new(&seg_path).exists() {
                            break;
                        }
                        let st = m.import_bundle(&seg_path).map_err(|e| e.to_string())?;
                        eprintln!("applied segment {fseg} ({} ops, {} skipped)", st.applied, st.skipped);
                        fseg += 1;
                        std::fs::write(&fcur_path, format!("{gen_id} {fseg}")).map_err(|e| e.to_string())?;
                    }
                }
                if once {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(interval));
            }
        }
        "restore" => {
            let from = need(&flags, "from")?;
            let until: Option<i64> = flag(&flags, "until-hlc").and_then(|v| v.parse().ok());
            let cursor = std::fs::read_to_string(format!("{from}/CURSOR"))
                .map_err(|_| "no CURSOR in stream dir".to_string())?;
            let gen_id = cursor.trim().split(' ').next().unwrap_or("").to_string();
            let dir = format!("{from}/gen-{gen_id}");
            let mut segs: Vec<String> = std::fs::read_dir(&dir)
                .map_err(|e| e.to_string())?
                .filter_map(|e| e.ok().map(|e| e.path().display().to_string()))
                .filter(|p| p.ends_with(".mgb"))
                .collect();
            segs.sort();
            let mut applied = 0usize;
            let mut meta_applied = 0usize;
            let mut meta_skipped = 0usize;
            for s in &segs {
                let st = m.import_bundle_until(s, until).map_err(|e| e.to_string())?;
                applied += st.applied;
                meta_applied += st.meta_applied;
                meta_skipped += st.meta_skipped;
            }
            let registry = if meta_applied > 0 {
                format!(", {meta_applied} registry entries")
            } else {
                String::new()
            };
            println!(
                "restored {} ops from {} segments (gen {gen_id}){registry}",
                applied,
                segs.len()
            );
            if until.is_some() && meta_skipped > 0 {
                eprintln!(
                    "note: {meta_skipped} registry entries (saved queries/templates/retention) \
                     skipped — a point-in-time restore replays grain history only"
                );
            }
        }
        "bundle" => {
            let out = need(&flags, "out")?;
            let since: i64 = flag(&flags, "since").and_then(|v| v.parse().ok()).unwrap_or(0);
            let st = m.bundle_since(since, &out).map_err(|e| e.to_string())?;
            println!(
                "bundled {} ops ({} bytes) → {} (next --since {})",
                st.ops, st.bytes, out, st.last_op_seq
            );
        }
        "import" => {
            let bundle = need(&flags, "bundle")?;
            let st = m.import_bundle(&bundle).map_err(|e| e.to_string())?;
            let registry = if st.meta_applied > 0 {
                format!(", {} registry entries", st.meta_applied)
            } else {
                String::new()
            };
            println!("applied {} ops, skipped {}{registry}", st.applied, st.skipped);
        }
        "migrate" => {
            let from = need(&flags, "from")?;
            let file = need(&flags, "file")?;
            // Bulk-load fast path: drop the FTS index for the duration and
            // rebuild once at the end — Turso indexes all existing rows at
            // CREATE INDEX time, instead of ~150ms of FTS bookkeeping per
            // write transaction.
            let deferred = m.defer_text_index().map_err(|e| e.to_string())?;
            let rep = run_migrate(&mut m, &ns, &from, &file, flag(&flags, "history"));
            if deferred {
                m.rebuild_text_index().map_err(|e| e.to_string())?;
            }
            let rep = rep?;
            for n in &rep.notes {
                eprintln!("areev: migrate: {n}");
            }
            println!("{}", rep.to_json());
        }
        "reindex" => {
            let n = m.rebuild_text_index().map_err(|e| e.to_string())?;
            println!(
                "text index rebuilt: {} grains indexed ({n} needed their text backfilled)",
                m.indexed_documents()
            );
            // Secondary indexes added after 1.0: reverse provenance, run
            // correlation, and related_to cross-links. A file written before
            // they existed answers provenance and run questions with nothing
            // until this runs.
            let links = m.rebuild_link_indexes().map_err(|e| e.to_string())?;
            println!("link indexes rebuilt: {links} rows (provenance, runs, cross-links)");
        }
        "related" => {
            let start = flag(&flags, "start").ok_or("related requires --start")?;
            let rels = areev_store::parse_relations(
                &flag(&flags, "relations").ok_or("related requires --relations")?,
            );
            if rels.is_empty() {
                return Err("--relations must name at least one relation".to_string());
            }
            let dir = Direction::parse(&flag(&flags, "direction").unwrap_or_default())
                .ok_or("--direction must be one of: out, in, both")?;
            let depth: usize = flag(&flags, "depth")
                .map_or(Ok(2), |d| d.parse())
                .map_err(|_| "--depth must be a number")?;
            let cap: usize = flag(&flags, "limit")
                .map_or(Ok(64), |l| l.parse())
                .map_err(|_| "--limit must be a number")?;
            let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
            let reached = m
                .related(&ns, &start, &refs, dir, depth, cap)
                .map_err(|e| e.to_string())?;
            if reached.is_empty() {
                println!("(nothing reachable from {start})");
            }
            for r in reached {
                println!("{r}");
            }
        }
        "entity-at" => {
            let subject = flag(&flags, "subject").ok_or("entity-at requires --subject")?;
            let relation = flag(&flags, "relation").ok_or("entity-at requires --relation")?;
            let at: i64 = flag(&flags, "at")
                .ok_or("entity-at requires --at (epoch ms)")?
                .parse()
                .map_err(|_| "--at must be epoch milliseconds")?;
            let axis = Axis::parse(&flag(&flags, "axis").unwrap_or_default())
                .ok_or("--axis must be one of: world, knowledge")?;
            match m
                .entity_at(&ns, &subject, &relation, at, axis)
                .map_err(|e| e.to_string())?
            {
                Some(g) => println!("{}", serde_json::to_string_pretty(&g).unwrap_or_default()),
                None => println!("(nothing known for {subject} {relation} at {at})"),
            }
        }
        "step-actions" => {
            let wf = flag(&flags, "workflow").ok_or("step-actions requires --workflow")?;
            let wf = Hash::from_hex(&wf).map_err(|e| e.to_string())?;
            let node = flag(&flags, "node");
            let limit: usize = flag(&flags, "limit")
                .map_or(Ok(64), |l| l.parse())
                .map_err(|_| "--limit must be a number")?;
            let rows = m
                .step_actions(&ns, &wf, node.as_deref(), limit)
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("(no execution records for {})", wf.to_hex());
            }
            for (n, h) in rows {
                println!("{n}\t{}", h.to_hex());
            }
        }
        "run-trace" => {
            let run = flag(&flags, "run-id").ok_or("run-trace requires --run-id")?;
            let limit: usize = flag(&flags, "limit")
                .map_or(Ok(64), |l| l.parse())
                .map_err(|_| "--limit must be a number")?;
            let trace = m.run_trace(&ns, &run, limit).map_err(|e| e.to_string())?;
            let produced = m.run_yield(&ns, &run, limit).map_err(|e| e.to_string())?;
            if trace.is_empty() {
                println!("(no grains recorded for run {run})");
            }
            println!("recorded during {run}: {} grain(s)", trace.len());
            for g in &trace {
                println!("  {} {:?}", g.hash.to_hex(), g.grain_type);
            }
            println!("produced by {run}: {} grain(s)", produced.len());
            for g in &produced {
                println!("  {} {:?}", g.hash.to_hex(), g.grain_type);
            }
        }
        "runs-touching" => {
            let h = flag(&flags, "hash").ok_or("runs-touching requires --hash")?;
            let h = Hash::from_hex(&h).map_err(|e| e.to_string())?;
            let depth: usize = flag(&flags, "depth")
                .map_or(Ok(4), |d| d.parse())
                .map_err(|_| "--depth must be a number")?;
            let runs = m.runs_touching(&ns, &h, depth).map_err(|e| e.to_string())?;
            if runs.is_empty() {
                println!("(no run produced or refined {})", h.to_hex());
            }
            for r in runs {
                println!("{r}");
            }
        }
        "verify" => {
            let rep = m.verify().map_err(|e| e.to_string())?;
            println!(
                "integrity: {} | grains: {} | hash mismatches: {} | undecodable: {}",
                rep.integrity, rep.grains, rep.hash_mismatches, rep.undecodable
            );
            if rep.integrity != "ok" || rep.hash_mismatches > 0 || rep.undecodable > 0 {
                return Err("verification FAILED".to_string());
            }
        }
        "stats" => {
            let s = m.stats().map_err(|e| e.to_string())?;
            println!(
                "grains: {} ({} current) | triples: {} | terms: {} | ops: {} | thread-indexed events: {}",
                s.grains, s.current, s.triples, s.terms, s.ops, s.events_indexed
            );
        }
        "serve" => {
            if !flags.contains_key("mcp") {
                return Err("only --mcp transport is available (areev serve --mcp)".to_string());
            }
            let mut facade = areev_cal::AreevFacade::with_session(m, Some(ns), None);
            report_meta_warnings(&facade);
            // Optional read-only mounts for cross-file ASSEMBLE:
            //   --mount alias=path[,alias=path...]
            // A recall/query in namespace "alias.inner" routes to the mount;
            // writes always stay on the primary file (mounts are read-only by
            // construction), so this only widens what recall/ASSEMBLE can read.
            if let Some(spec) = flag(&flags, "mount") {
                for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                    let (alias, path) = entry
                        .split_once('=')
                        .map(|(a, p)| (a.trim(), p.trim()))
                        .filter(|(a, p)| !a.is_empty() && !p.is_empty())
                        .ok_or_else(|| format!("--mount expects alias=path, got '{entry}'"))?;
                    let store =
                        Areev::open(path).map_err(|e| format!("mount '{alias}' ({path}): {e}"))?;
                    eprintln!("areev: mounted '{alias}' (read-only) → {path}");
                    facade.mount(alias, store);
                }
            }
            let facade = apply_principal(facade, &flags)?;
            let mut server = areev_mcp::McpServer::new(facade, None)
                .assembly_manifest_sample_rate(assembly_manifest_sample_rate(&flags)?);
            if flags.contains_key("no-destructive-ops") {
                server = server.allow_destructive_ops(false);
                eprintln!("areev: destructive operations disabled (read-only session)");
            }
            if let Some(lock) = flag(&flags, "lock-ns") {
                server = server.lock_namespace(lock.clone());
                eprintln!("areev: namespace locked to '{lock}' (per-call namespace ignored)");
            }
            if let Some(p) = flag(&flags, "profile") {
                let profile = areev_mcp::ToolProfile::parse(&p)?;
                server = server.tool_profile(profile);
                if profile == areev_mcp::ToolProfile::Memory {
                    eprintln!("areev: MCP tool profile 'memory' (run/loop tools unavailable)");
                }
            }
            // Host loop policy (--policy FILE or $AREEV_LOOP_POLICY): the
            // areev_loop tool honors the same grants as the CLI run. Host
            // config set at process start — never controllable by the client.
            if let Some(p) = load_policy(&flags)? {
                server = server.with_loop_policy(p);
                eprintln!("areev: loop host policy attached to areev_loop");
            }
            server.serve_stdio().map_err(|e| e.to_string())?;
        }
        "capture-stop" => {
            // Claude Code Stop-hook: JSON on stdin with session_id +
            // transcript_path. Store every ordered turn as a typed Event.
            use std::io::Read as IoRead;
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).map_err(|e| e.to_string())?;
            let hook: serde_json::Value =
                serde_json::from_str(&input).map_err(|e| format!("bad hook json: {e}"))?;
            let session = hook["session_id"].as_str().unwrap_or("unknown-session").to_string();
            let tpath = hook["transcript_path"]
                .as_str()
                .ok_or("hook json missing transcript_path")?;
            let transcript = std::fs::read_to_string(tpath).map_err(|e| e.to_string())?;
            // Claude's Stop hook points at the cumulative transcript, so every
            // firing re-reads turns already captured. Identify a turn by its
            // own content address rather than by its position in the thread: a
            // positional watermark counts grains any other writer put in the
            // same `(namespace, session)` — `areev remember --session-id`, a
            // binding, a second hook — and every foreign grain silently skips a
            // real turn. Re-adding a stored grain is already a free no-op, so
            // pinning `created_at` from the transcript makes the whole chain a
            // pure function of the transcript and the rerun idempotent by
            // construction.
            let policy_version = flag(&flags, "policy-version");
            let mut parent: Option<String> = None;
            let mut stored = 0;
            let mut skipped = 0;
            for (turn, line) in transcript.lines().enumerate() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let message = &v["message"];
                let Some(role) = message["role"].as_str() else { continue };
                let Some(role) = areev_core::types::Role::from_str(role) else {
                    continue;
                };
                let (text, blocks, elisions) =
                    transcript_content_blocks(&message["content"], turn);
                if text.trim().is_empty() && blocks.is_empty() {
                    continue;
                }
                let mut event = Event::new(&text)
                    .namespace(&ns)
                    .session(session.clone())
                    // Deterministic stamp: the transcript's own time when it
                    // has one, else the turn's position. Never the wall clock —
                    // that would re-hash every turn on every hook firing and
                    // defeat the dedup this relies on.
                    .created_at(transcript_turn_ms(&v).unwrap_or(turn as i64))
                    .role(role)
                    .content_blocks(blocks);
                if let Some(p) = parent.clone() {
                    event = event.parent_message(p);
                }
                if let Some(model) = message["model"].as_str().or_else(|| v["model"].as_str()) {
                    event = event.model(model.to_string());
                }
                if let Some(stop) = message["stop_reason"]
                    .as_str()
                    .or_else(|| v["stop_reason"].as_str())
                {
                    event = event.stop_reason(stop.to_string());
                }
                if let Some(usage) = transcript_token_usage(message) {
                    event = event.token_usage(usage);
                }
                if !elisions.is_empty() {
                    event
                        .common
                        .extra_fields
                        .insert("elisions".into(), serde_json::Value::Array(elisions));
                }
                // The policy in force when the turn happened. `areev corpus`
                // reports it under `binding.policy_versions`, which is how a
                // training row states which governance regime produced it —
                // without a producer that binding was always empty.
                if let Some(policy) = policy_version.as_deref() {
                    event.common.context = Some(serde_json::json!({
                        "policy_version": policy,
                    }));
                }
                // A hook capture is not a run; run_id deliberately stays None.
                let (_, hash) = areev_core::format::serialize::serialize_grain(&event)
                    .map_err(|e| e.to_string())?;
                if m.get(&hash).is_ok() {
                    skipped += 1;
                } else {
                    m.add(&event).map_err(|e| e.to_string())?;
                    stored += 1;
                }
                parent = Some(hash.to_hex());
            }
            println!("captured {stored} events for session {session} ({skipped} already stored)");
        }
        "recall-hook" => {
            // Claude Code UserPromptSubmit hook: read the hook JSON on stdin,
            // hybrid-search memory for the prompt, and print matching grains to
            // stdout — which Claude Code injects into the model's context. This
            // is the retrieval half of the learning loop, made automatic
            // (no reliance on the model deciding to call a recall tool).
            use std::io::Read as IoRead;
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).map_err(|e| e.to_string())?;
            let hook: serde_json::Value =
                serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
            // UserPromptSubmit carries `prompt`; SessionStart has none, so we
            // stay silent rather than inject noise.
            let query = hook["prompt"].as_str().unwrap_or("").trim().to_string();
            if query.is_empty() {
                return Ok(());
            }
            let k: usize = flag(&flags, "k").and_then(|v| v.parse().ok()).unwrap_or(5);
            // --with-loop additionally injects the pending recommendation
            // queue (a compact, capped block) so the loop closes into the
            // agent's context instead of waiting to be polled.
            let with_loop = flag(&flags, "with-loop").is_some();
            let grains = m
                .recall_hybrid(&ns, None, None, Some(&query), k, None)
                .map_err(|e| e.to_string())?;
            if grains.is_empty() && !with_loop {
                return Ok(());
            }
            if !grains.is_empty() {
                use areev_context::{ContextAssembler, FormatPolicy};
                let mut policy = FormatPolicy::claude();
                policy.token_budget =
                    Some(flag(&flags, "budget").and_then(|v| v.parse().ok()).unwrap_or(400));
                let hits: Vec<areev_cal::store_types::SearchHit> = grains
                    .into_iter()
                    .map(|grain| {
                        let hash = grain.hash;
                        areev_cal::store_types::SearchHit {
                            grain,
                            score: 1.0,
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
                    })
                    .collect();
                let ctx = ContextAssembler::new().format(&hits, &policy);
                if !ctx.text.trim().is_empty() {
                    println!("Relevant memory from Areev:\n{}", ctx.text);
                }
            }
            if with_loop {
                // Read-only listing over the same store handle (single writer
                // per file — never a second open). Engine-templated summaries;
                // origin=llm entries are labeled. Capped so a long queue can't
                // flood the prompt; the console/CLI remain the review surface.
                let sub = AreevSubstrate::new(m, Some(ns.to_string()));
                let engine = Engine::with_builtins();
                let mut pending = engine
                    .recommendations(&sub, Some(RecStatus::Pending))
                    .map_err(|e| e.to_string())?;
                if !pending.is_empty() {
                    pending.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.hash.cmp(&b.hash)));
                    const CAP: usize = 3;
                    println!(
                        "\nAreev Loop: {} pending recommendation(s) for this memory (review with `areev loop list`, act with approve/apply/reject --because):",
                        pending.len()
                    );
                    for r in pending.iter().take(CAP) {
                        let origin = match r.origin {
                            areev_loop::Origin::Llm { .. } => " [llm]",
                            areev_loop::Origin::Command { .. } => " [external]",
                            _ => "",
                        };
                        println!("  [{}] {}{}  {}", r.severity.as_str(), short(&r.hash), origin, r.summary.render());
                    }
                    if pending.len() > CAP {
                        println!("  … and {} more", pending.len() - CAP);
                    }
                }
            }
        }
        "remember" => run_remember(m, &ns, &flags)?,
        "memtool" => {
            let cmd = positional
                .first()
                .ok_or_else(|| "usage: areev memtool '<json>' --db <file>".to_string())?;
            let cmd: serde_json::Value =
                serde_json::from_str(cmd).map_err(|e| format!("bad command json: {e}"))?;
            let mut t = areev_store::memory_tool::MemoryTool::new(&mut m, &ns);
            println!("{}", t.execute(&cmd).map_err(|e| e.to_string())?);
        }
        "repl" => {
            use std::io::{BufRead, Write as IoWrite};
            let facade = areev_cal::AreevFacade::with_session(m, Some(ns.clone()), None);
            report_meta_warnings(&facade);
            let facade = apply_principal(facade, &flags)?;
            let ex = CalExecutor::new(CalExecutorConfig {
                allow_destructive_ops: !flags.contains_key("no-destructive-ops"),
                assembly_manifest_sample_rate: assembly_manifest_sample_rate(&flags)?,
                ..CalExecutorConfig::default()
            })
            .with_governance(std::sync::Arc::new(areev_loop_adapter::LoopGovernance::new()));
            eprintln!("areev repl — namespace '{ns}' · CAL statements, or .stats .log .help .quit");
            let stdin = std::io::stdin();
            let mut lines = stdin.lock().lines();
            loop {
                eprint!("cal> ");
                std::io::stderr().flush().ok();
                let line = match lines.next() {
                    Some(Ok(l)) => l,
                    _ => break,
                };
                let line = line.trim();
                match line {
                    "" => continue,
                    ".quit" | ".exit" => break,
                    ".help" => eprintln!(
                        "CAL: RECALL / ASSEMBLE / EXISTS / HISTORY / ADD / SUPERSEDE / DESCRIBE / | COUNT\n\
                         dot: .stats  .log  .verify  .quit"
                    ),
                    ".stats" => match facade.with_store(|st| st.stats()) {
                        Ok(s) => eprintln!(
                            "grains {} ({} current) · triples {} · ops {}",
                            s.grains, s.current, s.triples, s.ops
                        ),
                        Err(e) => eprintln!("error: {e}"),
                    },
                    ".log" => match facade.with_store(|st| st.changes_since(0, 20)) {
                        Ok(ops) => {
                            for o in ops {
                                eprintln!("{:>5}  {:<9}  {}", o.op_seq, o.op, o.hash.to_hex());
                            }
                        }
                        Err(e) => eprintln!("error: {e}"),
                    },
                    ".verify" => match facade.with_store(|st| st.verify()) {
                        Ok(r) => eprintln!(
                            "integrity {} · {} grains · {} mismatches",
                            r.integrity, r.grains, r.hash_mismatches
                        ),
                        Err(e) => eprintln!("error: {e}"),
                    },
                    q => match ex.execute(q, &facade) {
                        Ok(res) => {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&res.result).unwrap_or_default()
                            );
                            for w in res.warnings {
                                eprintln!("warning: {w}");
                            }
                        }
                        Err(e) => eprintln!("error: {e}"),
                    },
                }
            }
        }
        "ui" => {
            let addr = flag(&flags, "addr").unwrap_or_else(|| "127.0.0.1:7437".to_string());
            let allow_remote = flag(&flags, "allow-remote").is_some();

            // Optional console authentication. `--token-env <VAR>` names an
            // environment variable holding the shared secret, keeping it out of
            // argv and shell history (same convention as --passphrase-env).
            // When set, every request needs it: browsers via the native HTTP
            // Basic prompt (any username, password = token), scripts via
            // `Authorization: Bearer <token>`.
            let auth_token = match flag(&flags, "token-env") {
                Some(var) => {
                    let t = std::env::var(&var).map_err(|_| {
                        format!("--token-env {var}: environment variable is not set")
                    })?;
                    if t.trim().is_empty() {
                        return Err(format!("--token-env {var}: token is empty"));
                    }
                    // [A1] The shared secret's entropy is the ONLY control on
                    // this path, and nothing here can measure it — but a
                    // token Areev did not mint is one an operator chose, and
                    // operator-chosen secrets are the reason the stored
                    // digest is worth grinding at all. Warn, never refuse:
                    // an existing deployment must not break on upgrade.
                    if !areev_core::authz::token_is_minted(&t) {
                        eprintln!(
                            "areev: ⚠ --token-env {var} holds a token Areev did not mint, so \
                             its entropy is unknown. Prefer `areev auth mint` (256-bit) and a \
                             per-principal credential map (--auth), which is also the only \
                             way approvals get an attributable identity."
                        );
                    }
                    Some(t)
                }
                None => None,
            };
            let has_auth = auth_token.is_some();

            if !addr_is_loopback(&addr) && !allow_remote {
                let why = if has_auth {
                    "It is authenticated, but still plaintext HTTP — the token and all \
                     memory cross the wire in the clear. Terminate TLS in front of it, \
                     or pass --allow-remote to accept that risk."
                } else {
                    "With no --token-env it is an UNAUTHENTICATED, writable console over \
                     plaintext HTTP — anyone who can reach it could read or modify your \
                     memory. Add --token-env <VAR>, keep it on loopback, put it behind a \
                     TLS-terminating proxy, or pass --allow-remote to override."
                };
                return Err(format!(
                    "refusing to bind the console to a non-loopback address ({addr}). {why}"
                ));
            }

            let facade = areev_cal::AreevFacade::with_session(m, Some(ns), None);
            report_meta_warnings(&facade);
            let mut server = areev_server::UiServer::new(facade, db.clone());
            if let Some(path) = flag(&flags, "auth") {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("--auth {path}: {e}"))?;
                let map = areev_core::authz::CredentialMap::from_json(&text)
                    .map_err(|e| e.to_string())?;
                eprintln!(
                    "areev: credential map loaded ({} token(s)); rights come from the \
                     file's grant grains; unauthenticated requests run as 'anonymous'",
                    map.tokens.len()
                );
                // [A1] Name what is about to expire, every start. A console
                // that first mentions an expiry at the moment it starts
                // refusing is a console that mentions it during an incident.
                let now = areev_core::time::now_ms();
                for (id, when) in map.expiring_within(now, 14 * 86_400_000) {
                    let entry = map.tokens.iter().find(|t| t.id() == id);
                    if entry.is_some_and(|t| t.is_expired_at(now)) {
                        eprintln!(
                            "areev: ⚠ credential {id} EXPIRED {when} — it authenticates nobody; \
                             mint a replacement (areev auth mint) and revoke it"
                        );
                    } else {
                        eprintln!("areev: credential {id} expires {when} (within 14 days)");
                    }
                }
                server = server.with_credentials(map);
            }
            if allow_remote {
                server = server.allow_remote(true);
            }
            // Cross-origin POST allowlist (issue #125). The console's Origin
            // drive-by check is NOT lifted by --allow-remote: HTTP Basic is
            // cached by the browser and re-attached cross-site, so Origin is
            // the only thing telling the console's own page apart from an
            // attacker's page riding a viewer's cached credential.
            // --allow-origin names the exact origin(s) (the reverse proxy's
            // public hostname) permitted to POST cross-origin instead —
            // comma-separated for more than one, no wildcards, no subdomain
            // matching. (`parse_args` overwrites a repeated flag rather than
            // accumulating it, so comma-separation is the only list form.)
            if let Some(raw) = flag(&flags, "allow-origin") {
                fn validate_origin(o: &str) -> Result<String, String> {
                    let o = o.trim();
                    if o.is_empty() {
                        return Err("--allow-origin: empty origin".to_string());
                    }
                    if o == "*" {
                        return Err(
                            "--allow-origin: '*' is not accepted — name the exact origin(s) \
                             allowed to POST"
                                .to_string(),
                        );
                    }
                    let (scheme, rest) = if let Some(r) = o.strip_prefix("https://") {
                        ("https", r)
                    } else if let Some(r) = o.strip_prefix("http://") {
                        ("http", r)
                    } else {
                        return Err(format!("--allow-origin {o}: must start with http:// or https://"));
                    };
                    if rest.contains('@') {
                        return Err(format!("--allow-origin {o}: must not contain userinfo"));
                    }
                    if rest.contains('?') || rest.contains('#') {
                        return Err(format!("--allow-origin {o}: must not contain a query or fragment"));
                    }
                    let host = rest.strip_suffix('/').unwrap_or(rest);
                    if host.is_empty() {
                        return Err(format!("--allow-origin {o}: missing host"));
                    }
                    if host.contains('/') {
                        return Err(format!(
                            "--allow-origin {o}: must not contain a path beyond an optional \
                             trailing '/'"
                        ));
                    }
                    Ok(format!("{scheme}://{host}"))
                }

                let mut origins = Vec::new();
                for part in raw.split(',') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    let normalized = validate_origin(part)?;
                    if normalized.starts_with("http://")
                        && !addr_is_loopback(normalized.trim_start_matches("http://"))
                    {
                        eprintln!(
                            "areev: WARNING — --allow-origin {normalized} is http:// \
                             (non-TLS) and non-loopback; a Basic credential the browser \
                             caches for that origin would cross the wire in the clear."
                        );
                    }
                    server = server.allow_origin(normalized.clone());
                    origins.push(normalized);
                }
                if origins.is_empty() {
                    return Err("--allow-origin: no origins given".to_string());
                }
                eprintln!("areev: cross-origin POSTs accepted from: {}", origins.join(", "));
            }
            if flags.contains_key("no-destructive-ops") {
                server = server.allow_destructive_ops(false);
                eprintln!("areev: destructive operations disabled (read-only console)");
            }
            if let Some(tok) = auth_token {
                server = server.with_auth(tok);
                eprintln!(
                    "areev: console authentication ENABLED — HTTP Basic \
                     (any username, password = the token)"
                );
            } else {
                eprintln!(
                    "areev: console is UNAUTHENTICATED — enable with --auth <map> \
                     (per-principal, attributable; `areev auth mint` creates one) or \
                     --token-env <VAR> (one shared secret, cannot approve)"
                );
            }
            // Host loop policy (--policy FILE or $AREEV_LOOP_POLICY): a
            // console-triggered loop run honors the same grants as the CLI
            // run. Never grantable from the console itself.
            if let Some(p) = load_policy(&flags)? {
                server = server.with_loop_policy(p);
                eprintln!("areev: loop host policy attached to the console's loop routes");
            }
            // [A0] Whether a proxy-asserted identity may answer a HITL
            // approval. Parsed BEFORE the SSO arm below so an invalid value —
            // or one given without SSO configured at all — is an error rather
            // than a silently-ignored security flag.
            let sso_approvals = match flag(&flags, "sso-approvals").as_deref() {
                None | Some("deny") => false,
                Some("allow") => true,
                Some(other) => {
                    return Err(format!(
                        "--sso-approvals {other}: expected 'deny' (default) or 'allow'"
                    ))
                }
            };
            if flag(&flags, "sso-approvals").is_some() && flag(&flags, "sso-header").is_none() {
                return Err(
                    "--sso-approvals applies to trusted-header SSO only — it has no effect \
                     without --sso-header/--sso-secret-env. Credential-map principals (--auth) \
                     may always approve; shared-token and anonymous callers never may."
                        .into(),
                );
            }
            // SSO v0 — trusted-header auth: an authenticating proxy does
            // OIDC/SAML and forwards identity in --sso-header; honored only
            // with the proxy shared secret (--sso-secret-env) in
            // x-areev-proxy-secret. Rights come from the file's grants.
            match (flag(&flags, "sso-header"), flag(&flags, "sso-secret-env")) {
                (Some(header), Some(var)) => {
                    let secret = std::env::var(&var)
                        .map_err(|_| format!("--sso-secret-env {var}: environment variable is not set"))?;
                    if secret.trim().is_empty() {
                        return Err(format!("--sso-secret-env {var}: secret is empty"));
                    }
                    // The rotation window (#79): while --sso-secret-env-next
                    // is set, either secret proves the proxy, so a fleet
                    // moves over one node at a time instead of atomically.
                    let next = match flag(&flags, "sso-secret-env-next") {
                        None => None,
                        Some(nvar) => {
                            let v = std::env::var(&nvar).map_err(|_| {
                                format!(
                                    "--sso-secret-env-next {nvar}: environment variable is not set"
                                )
                            })?;
                            if v.trim().is_empty() {
                                return Err(format!(
                                    "--sso-secret-env-next {nvar}: secret is empty"
                                ));
                            }
                            if v == secret {
                                return Err(format!(
                                    "--sso-secret-env-next {nvar} holds the same value as \
                                     --sso-secret-env {var} — that is a rotation that did not \
                                     happen, and it would read as 'both live' everywhere"
                                ));
                            }
                            Some(v)
                        }
                    };
                    let rotating = next.is_some();
                    server = server.with_sso_rotating(&header, secret, next);
                    eprintln!(
                        "areev: trusted-header SSO enabled ({header} + x-areev-proxy-secret)"
                    );
                    server = server.with_sso_approvals(sso_approvals);
                    // [A2] Groups → principals, so SSO scales without a grant
                    // grain per person. Resolved through --auth's `groups`
                    // table; without a map there is nothing to resolve
                    // against, so say so rather than failing silently.
                    if let Some(gh) = flag(&flags, "sso-groups-header") {
                        if flag(&flags, "auth").is_none() {
                            return Err(
                                "--sso-groups-header needs --auth <map>: the group → principal \
                                 table lives in the credential map, and without one every group \
                                 would resolve to nothing"
                                    .into(),
                            );
                        }
                        server = server.with_sso_groups_header(&gh);
                        eprintln!(
                            "areev: SSO groups honored via {gh} (group-derived principals are \
                             roles: they may never answer a HITL approval)"
                        );
                    }
                    if let Some(prefix) = flag(&flags, "sso-principal-prefix") {
                        if prefix.trim().is_empty() {
                            return Err("--sso-principal-prefix: empty prefix".into());
                        }
                        server = server.with_sso_principal_prefix(&prefix);
                        eprintln!("areev: proxy-asserted principals are prefixed {prefix:?}");
                    }
                    if sso_approvals {
                        eprintln!(
                            "areev: ⚠ --sso-approvals allow — a proxy-asserted identity may \
                             answer HITL approvals. Every approval's audit record is then only \
                             as strong as the proxy secret: anyone holding it can approve as \
                             anyone. Prefer per-principal credentials (--auth) for approvers."
                        );
                    } else {
                        eprintln!(
                            "areev: SSO identities may review but NOT approve (run.respond); \
                             approvers need a per-principal credential (--auth). Override with \
                             --sso-approvals allow."
                        );
                    }
                    if rotating {
                        // Loud, every start: a rotation window left open is
                        // an extra impersonation-grade credential live in
                        // production, and the whole risk of the feature is
                        // that nobody remembers to close it.
                        eprintln!(
                            "areev: ⚠ SSO secret ROTATION WINDOW open — two proxy secrets are \
                             accepted. Retire the old one (drop --sso-secret-env-next and \
                             promote its value) as soon as every proxy presents the new one: \
                             docs/runbooks/sso-secret-rotation.md"
                        );
                    }
                }
                (None, None) => {}
                _ => return Err("--sso-header and --sso-secret-env must be given together".into()),
            }
            // [A3] Native OIDC (`oidc` build feature): the console runs the
            // authorization-code+PKCE flow itself and issues a session
            // cookie. Unlike --sso-header, the identity is proven by a
            // signature over the issuer's published key set — which is why an
            // OIDC principal may approve without --sso-approvals.
            if let Some(issuer) = flag(&flags, "oidc-issuer") {
                    #[cfg(feature = "oidc")]
                    {
                        let client_id = flag(&flags, "oidc-client-id").ok_or(
                            "--oidc-issuer needs --oidc-client-id".to_string(),
                        )?;
                        let secret_var = flag(&flags, "oidc-client-secret-env").ok_or(
                            "--oidc-issuer needs --oidc-client-secret-env VAR (the secret is \
                             named, never passed on the command line)"
                                .to_string(),
                        )?;
                        let client_secret = std::env::var(&secret_var).map_err(|_| {
                            format!(
                                "--oidc-client-secret-env {secret_var}: environment variable is \
                                 not set"
                            )
                        })?;
                        if client_secret.trim().is_empty() {
                            return Err(format!(
                                "--oidc-client-secret-env {secret_var}: secret is empty"
                            ));
                        }
                        let redirect_uri = flag(&flags, "oidc-redirect-uri").ok_or(
                            "--oidc-issuer needs --oidc-redirect-uri (registered VERBATIM with \
                             the provider — RFC 9700 requires exact matching)"
                                .to_string(),
                        )?;
                        if !issuer.starts_with("https://") {
                            return Err(format!("--oidc-issuer {issuer}: must be https"));
                        }
                        let cfg = areev_server::oidc::OidcConfig {
                            issuer,
                            client_id,
                            client_secret,
                            redirect_uri: redirect_uri.clone(),
                            scopes: flag(&flags, "oidc-scopes")
                                .unwrap_or_else(|| "openid email profile".to_string()),
                            principal_claim: flag(&flags, "oidc-principal-claim")
                                .unwrap_or_else(|| "email".to_string()),
                            principal_prefix: flag(&flags, "oidc-principal-prefix"),
                        };
                        // Discovery happens NOW: a console that cannot reach
                        // its IdP should fail to start loudly, not fail every
                        // login later with the operator none the wiser.
                        let provider = areev_server::oidc::OidcProvider::discover(cfg)
                            .map_err(|e| e.to_string())?;
                        eprintln!("areev: native OIDC enabled — login at /auth/login");
                        if !redirect_uri.starts_with("https://") {
                            eprintln!(
                                "areev: ⚠ --oidc-redirect-uri is not https, so the session \
                                 cookie cannot be marked Secure. Acceptable on loopback; on any \
                                 real deployment terminate TLS first."
                            );
                        }
                        server = server.with_oidc(provider);
                    }
                    #[cfg(not(feature = "oidc"))]
                    {
                        let _ = issuer;
                        return Err(
                            "this build has no native OIDC — rebuild with `--features oidc`, or \
                             use trusted-header SSO behind an authenticating proxy \
                             (--sso-header), which is the documented default"
                                .into(),
                        );
                    }
            }
            // Native TLS (`tls` build feature): --tls-cert/--tls-key PEM
            // paths. The documented default remains a TLS-terminating
            // proxy; this is for deployments with nowhere to put one.
            match (flag(&flags, "tls-cert"), flag(&flags, "tls-key")) {
                (Some(cert), Some(key)) => {
                    #[cfg(feature = "tls")]
                    {
                        server = server.with_tls(&cert, &key).map_err(|e| e.to_string())?;
                        eprintln!("areev: native TLS enabled (cert {cert})");
                    }
                    #[cfg(not(feature = "tls"))]
                    {
                        let _ = (&cert, &key);
                        return Err(
                            "this build has no native TLS — rebuild with `--features tls`, \
                             or use the documented TLS-terminating proxy"
                                .into(),
                        );
                    }
                }
                (None, None) => {}
                _ => return Err("--tls-cert and --tls-key must be given together".into()),
            }
            let listener = areev_server::UiServer::bind(&addr).map_err(|e| e.to_string())?;
            if !addr_is_loopback(&addr) {
                eprintln!(
                    "areev: WARNING — bound to non-loopback {addr} over plaintext HTTP; {}",
                    if has_auth {
                        "the token crosses the wire in the clear — use a TLS-terminating proxy."
                    } else {
                        "UNAUTHENTICATED — anyone who can reach it can read/modify memory."
                    }
                );
            }
            eprintln!(
                "areev console → http://{}  (Ctrl-C to stop)",
                listener.local_addr().map_err(|e| e.to_string())?
            );
            server.serve(listener).map_err(|e| e.to_string())?;
        }
        "get" => {
            let h = positional
                .first()
                .ok_or_else(|| "usage: areev get <hash> --db <file>".to_string())?;
            let hash = Hash::from_hex(h).map_err(|e| e.to_string())?;
            let g = m.get(&hash).map_err(|e| e.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "hash": g.hash.to_hex(),
                    "type": format!("{:?}", g.grain_type).to_lowercase(),
                    "fields": g.fields,
                })
            );
        }
        "provenance" => {
            // Reverse provenance: grains distilled from a source (credit
            // assignment / episode-scoped unlearn). Forward provenance — a
            // grain's own `derived_from` — is already visible via `areev get`.
            let h = flag(&flags, "of")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| {
                    "usage: areev provenance <source-hash> --db <file>  \
                     (lists grains whose derived_from is that hash)"
                        .to_string()
                })?;
            let h = h.strip_prefix("sha256:").unwrap_or(&h);
            let parent = Hash::from_hex(h).map_err(|e| e.to_string())?;
            let kids = m.grains_derived_from(&parent).map_err(|e| e.to_string())?;
            for g in kids {
                println!(
                    "{}",
                    serde_json::json!({
                        "hash": g.hash.to_hex(),
                        "type": format!("{:?}", g.grain_type).to_lowercase(),
                        "subject": g.get_str("subject"),
                        "relation": g.get_str("relation"),
                        "object": g.get_str("object"),
                    })
                );
            }
        }
        "forks" => {
            // Surface open forks: (ns, subject, relation) with >1 live head,
            // from concurrent supersession of the same value (e.g. edits synced
            // from two writers). Both tips are kept — nothing is lost — until an
            // explicit `areev merge` closes the fork.
            let forks = m.open_forks().map_err(|e| e.to_string())?;
            if forks.is_empty() {
                eprintln!("no open forks");
            }
            for f in forks {
                println!(
                    "{}",
                    serde_json::json!({
                        "namespace": f.namespace,
                        "subject": f.subject,
                        "relation": f.relation,
                        "heads": f.heads.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
                    })
                );
            }
        }
        "subject-report" => {
            // The DSAR read (GDPR Art. 15 access / Art. 20 portability):
            // everything forget-subject WOULD erase — the same selector in
            // show-me mode. JSONL to stdout (or --out), optionally plus a
            // portable subject-scoped bundle (--bundle). Read-only: no
            // --yes, and --as needs only the read verb.
            let subject = positional.first().cloned().or_else(|| flag(&flags, "subject")).ok_or(
                "usage: areev subject-report <subject> [--ns NS] [--text-mentions] \
                 [--out FILE] [--bundle FILE]"
                    .to_string(),
            )?;
            let opts = areev_store::ErasureOptions {
                text_mentions: flag(&flags, "text-mentions")
                    .map(|v| !matches!(v.as_str(), "false" | "0" | "off" | "no"))
                    .unwrap_or(false),
            };
            if let Some(principal) = flag(&flags, "as") {
                let grants = m.authz_grants(&principal).map_err(|e| e.to_string())?;
                areev_core::authz::AuthzSet::restricted(&principal, grants)
                    .check(areev_core::authz::Verb::Read, &ns)
                    .map_err(|e| e.to_string())?;
            }
            let report = m.subject_report_with(&ns, &subject, opts).map_err(|e| e.to_string())?;
            let mut lines = String::new();
            for g in &report.grains {
                let fields: serde_json::Map<String, serde_json::Value> =
                    g.fields.clone().into_iter().collect();
                let row = serde_json::json!({
                    "hash": g.hash.to_hex(),
                    "type": g.grain_type.as_str(),
                    "fields": fields,
                });
                lines.push_str(&row.to_string());
                lines.push('\n');
            }
            match flag(&flags, "out") {
                Some(path) => {
                    std::fs::write(&path, &lines).map_err(|e| e.to_string())?;
                    println!("wrote {} grains to {path}", report.grains.len());
                }
                None => print!("{lines}"),
            }
            if let Some(bpath) = flag(&flags, "bundle") {
                let stats = m
                    .subject_bundle_with(&ns, &subject, opts, &bpath)
                    .map_err(|e| e.to_string())?;
                eprintln!("bundle: {} records, {} bytes -> {bpath}", stats.ops, stats.bytes);
            }
            eprintln!(
                "subject-report '{subject}' ns '{ns}': {} grains; matched identities: {}",
                report.grains.len(),
                report.identity_names.join(", ")
            );
        }
        "forget-subject" => {
            // Right-to-erasure for one identity: erase every grain holding a
            // structured reference to the subject (history included), its
            // thread events, its dictionary entry, and erased-only
            // vocabulary — with replicating tombstones. Destructive and
            // bulk, so it demands an explicit --yes.
            let subject = positional.first().cloned().or_else(|| flag(&flags, "subject")).ok_or(
                "usage: areev forget-subject <subject> [--ns NS] [--text-mentions] --yes"
                    .to_string(),
            )?;
            if flags.contains_key("no-destructive-ops") {
                return Err(
                    "destructive operations are disabled for this invocation \
                     (--no-destructive-ops)"
                        .into(),
                );
            }
            if !flags.contains_key("yes") {
                return Err(format!(
                    "forget-subject erases EVERY grain referencing '{subject}' in namespace \
                     '{ns}' (history included) and replicates the erasure to peers. \
                     Re-run with --yes to proceed."
                ));
            }
            // Same truthiness idiom as --index-text: bare presence enables,
            // an explicit falsy value disables — "--text-mentions false"
            // must not silently widen the erasure.
            let opts = areev_store::ErasureOptions {
                text_mentions: flag(&flags, "text-mentions")
                    .map(|v| !matches!(v.as_str(), "false" | "0" | "off" | "no"))
                    .unwrap_or(false),
            };
            if let Some(principal) = flag(&flags, "as") {
                let grants = m.authz_grants(&principal).map_err(|e| e.to_string())?;
                areev_core::authz::AuthzSet::restricted(&principal, grants)
                    .check(areev_core::authz::Verb::Erase, &ns)
                    .map_err(|e| e.to_string())?;
            }
            let rep = m.forget_subject_with(&ns, &subject, opts).map_err(|e| e.to_string())?;
            let stale_exports = m
                .corpus_exports_touching_subject(&subject)
                .unwrap_or_else(|e| {
                    eprintln!("areev: warning: could not consult corpus registry: {e}");
                    Vec::new()
                });
            // The CLI is a host, and hosts audit (REQ-ERASE-5 leaves the
            // record to the host precisely so the engine never writes the
            // identity back). Same builder as the CAL path, so `areev audit
            // export` sees one shape whichever surface erased.
            write_erasure_audit(
                &mut m,
                &flags,
                "erase",
                // A fingerprint, never the identity — the audit grain
                // outlives the erasure and replicates.
                &format!(
                    "subject:{} ns:{ns}",
                    areev_core::authz::subject_fingerprint(&subject)
                ),
                rep.grains_erased,
                &stale_exports,
            );
            println!(
                "erased {} grains ({} dictionary entries, {} vocabulary tokens, {} blobs reclaimed)",
                rep.grains_erased, rep.terms_removed, rep.vocab_removed, rep.blobs_reclaimed
            );
        }
        "purge-older-than" => {
            // Retention sweep: erase grains older than N days (created_at),
            // optionally limited to one grain type. --ns "" sweeps every
            // namespace. Destructive and bulk: demands --yes.
            let days: i64 = positional
                .first()
                .and_then(|v| v.parse().ok())
                .or_else(|| flag(&flags, "days").and_then(|v| v.parse().ok()))
                .ok_or("usage: areev purge-older-than <days> [--ns NS] [--type event] --yes".to_string())?;
            let gt = match flag(&flags, "type") {
                Some(t) => Some(
                    areev_core::types::GrainType::from_str(&t)
                        .ok_or_else(|| format!("unknown grain type '{t}'"))?,
                ),
                None => None,
            };
            if flags.contains_key("no-destructive-ops") {
                return Err(
                    "destructive operations are disabled for this invocation \
                     (--no-destructive-ops)"
                        .into(),
                );
            }
            if !flags.contains_key("yes") {
                let scope = if ns.is_empty() {
                    "across EVERY namespace".to_string()
                } else {
                    format!("in namespace '{ns}'")
                };
                return Err(format!(
                    "purge-older-than erases every{} grain older than {days} days {scope} \
                     and replicates the erasure to peers. Re-run with --yes to proceed.",
                    gt.map(|g| format!(" {g:?}")).unwrap_or_default()
                ));
            }
            let cutoff = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
                - days * 24 * 3600 * 1000;
            let ns_opt = if ns.is_empty() { None } else { Some(ns.as_str()) };
            if let Some(principal) = flag(&flags, "as") {
                let grants = m.authz_grants(&principal).map_err(|e| e.to_string())?;
                // An all-namespace sweep needs erase on `*`.
                areev_core::authz::AuthzSet::restricted(&principal, grants)
                    .check(areev_core::authz::Verb::Erase, ns_opt.unwrap_or("*"))
                    .map_err(|e| e.to_string())?;
            }
            let exports = m.corpus_exports().unwrap_or_else(|e| {
                eprintln!("areev: warning: could not consult corpus registry: {e}");
                Vec::new()
            });
            let rep = m.forget_older_than(ns_opt, cutoff, gt).map_err(|e| e.to_string())?;
            let stale_exports =
                areev_store::exports_touching_hashes(&exports, &rep.erased_hashes);
            write_erasure_audit(
                &mut m,
                &flags,
                "erase",
                &format!(
                    "older_than:{days}d ns:{}{}",
                    if ns.is_empty() { "*" } else { ns.as_str() },
                    flag(&flags, "type").map(|t| format!(" type:{t}")).unwrap_or_default()
                ),
                rep.grains_erased,
                &stale_exports,
            );
            println!(
                "erased {} grains ({} vocabulary tokens, {} blobs reclaimed)",
                rep.grains_erased, rep.vocab_removed, rep.blobs_reclaimed
            );
        }
        "merge" => {
            // Close an open fork by writing a resolved value that supersedes
            // every tip; the merge grain records all parents in its blob.
            let s = need(&flags, "subject")?;
            let r = need(&flags, "relation")?;
            let o = need(&flags, "object")?;
            let conf: f64 = flag(&flags, "confidence")
                .map(|c| c.parse().map_err(|_| "bad --confidence".to_string()))
                .transpose()?
                .unwrap_or(0.9);
            let mut merged = Fact::new(&s, &r, &o).confidence(conf);
            merged.common.namespace = Some(ns.clone());
            let h = m
                .merge_heads(&ns, &s, &r, &mut merged)
                .map_err(|e| e.to_string())?;
            println!("{h}");
        }
        "novelty" => {
            // Advise-mode paraphrase novelty check: nearest existing grains to
            // a candidate text (needs --embed-cmd). A reflection harness reads
            // the top similarity and decides to supersede vs add — the engine
            // never drops anything on its own.
            let text = need(&flags, "text")?;
            let subject = flag(&flags, "subject");
            let relation = flag(&flags, "relation");
            let k: usize = flag(&flags, "k").and_then(|v| v.parse().ok()).unwrap_or(5);
            let matches = m
                .nearest_semantic(&ns, subject.as_deref(), relation.as_deref(), &text, k)
                .map_err(|e| e.to_string())?;
            for (h, sim) in matches {
                let object = m.get(&h).ok().and_then(|g| g.get_str("object").map(String::from));
                println!(
                    "{}",
                    serde_json::json!({
                        "hash": h.to_hex(),
                        "similarity": (sim * 1000.0).round() / 1000.0,
                        "object": object,
                    })
                );
            }
        }
        "init" => {
            run_init(m, &ns, &flags, &positional)?;
        }
        "loop" => {
            run_loop(m, &ns, &flags, &positional)?;
        }
        "audit" => {
            run_audit(&mut m, &flags, &positional)?;
        }
        "anonymize" => run_anonymize(m, &flags, &positional)?,
        "trigger" => {
            return trigger_cli::run_trigger(m, &ns, &flags, &positional);
        }
        "retention" => {
            run_retention(&mut m, &ns, &flags, &positional)?;
        }
        "hold" => {
            run_hold(&mut m, &ns, &flags, &positional)?;
        }
        // ── areev run: the governed workflow runtime (governed-agents §8) ──
        "run" => {
            return run_run(m, &db, &ns, &flags, &positional);
        }
        // ── areev eval: evalsets + the §7.4 gating edge ──
        "eval" => {
            return run_eval(m, &flags, &positional);
        }
        // ── areev tool provenance: one-command code forensics (§7.4) ──
        "tool" => {
            let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("");
            let hash_arg = positional.get(1).cloned();
            if sub_cmd != "provenance" || hash_arg.is_none() {
                return Err("usage: areev tool provenance <hash> [--depth N]".into());
            }
            return run_tool_provenance(m, &ns, &hash_arg.unwrap(), &flags);
        }
        "blob" => {
            // `blob put` writes; `blob get` normally never reaches here (it is
            // served pre-open above) — it lands here only for a sealed blob or
            // a Postgres memory, both of which genuinely need the open handle.
            match positional.first().map(String::as_str).unwrap_or("") {
                "put" => {
                    let bytes = match positional.get(1) {
                        Some(path) => std::fs::read(path)
                            .map_err(|e| format!("blob put: reading {path}: {e}"))?,
                        None => {
                            if !flags.contains_key("stdin") {
                                return Err("usage: areev blob put <FILE> --db <file>  (or --stdin)".into());
                            }
                            use std::io::Read;
                            let mut buf = Vec::new();
                            std::io::stdin()
                                .read_to_end(&mut buf)
                                .map_err(|e| format!("blob put: reading stdin: {e}"))?;
                            buf
                        }
                    };
                    // Idempotent by construction: the address is the content.
                    println!("{}", m.put_blob(&bytes).map_err(|e| e.to_string())?);
                }
                "get" => {
                    let uri = positional
                        .get(1)
                        .ok_or("usage: areev blob get <cas-uri> --db <file>")?;
                    use std::io::Write;
                    let bytes = m.get_blob(uri).map_err(|e| e.to_string())?;
                    std::io::stdout().write_all(&bytes).map_err(|e| e.to_string())?;
                }
                other => {
                    return Err(format!(
                        "unknown blob subcommand '{other}' — try `areev blob put|get`"
                    ))
                }
            }
        }
        "blobs" => {
            // `areev blobs encrypt` — migrate CAS attachments written before
            // sidecar encryption landed (GDPR Art. 32; docs/gdpr.md §4).
            // New attachments are sealed on write, so this is a one-time
            // catch-up for existing files. Idempotent.
            let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("");
            if sub_cmd != "encrypt" {
                return Err("usage: areev blobs encrypt --db <file> --passphrase-env VAR".into());
            }
            let (encrypted, already) = m.encrypt_blobs().map_err(|e| e.to_string())?;
            println!(
                "encrypted {encrypted} attachment(s); {already} were already encrypted"
            );
        }
        other => return Err(format!("unknown command '{other}' — try `areev help`")),
    }
    Ok(())
}

/// `areev retention <set|list|clear|sweep>` — declarative storage limitation
/// (GDPR Art. 5(1)(e)).
///
/// Policy is a **file-truth** (`retention:<ns>` meta rows), so it travels
/// with the memory: a copy, a sync, or a restore on another host still says
/// "events here live 90 days". Declaring is separate from enforcing —
/// `set` never deletes anything; `sweep` does, and demands `--yes` like
/// every other bulk erasure. The governed alternative is the loop's
/// `retention_sweep` analyzer, which proposes the same purge through review
/// and audit instead of performing it.
/// A declarative anonymization fixture set (issue #47).
///
/// ```json
/// {
///   "policy": { "mode": "egress", "categories": { "sg_nric": "redact" } },
///   "must_redact":     ["NRIC S1234567D", "MRN 00456123"],
///   "must_not_redact": ["invoice 4471820", "2026-08-19"]
/// }
/// ```
///
/// Deliberately symmetric: a policy that redacts everything passes
/// `must_redact` trivially, so the `must_not_redact` half is what stops an
/// over-broad policy from looking correct.
///
/// Read with the `serde_json` value API rather than a derive — this crate
/// hand-rolls its argument parsing and carries no `serde` derive dependency.
fn fixture_strings(root: &serde_json::Value, key: &str, path: &str) -> Result<Vec<String>, String> {
    match root.get(key) {
        None => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{path}: every \"{key}\" entry must be a string"))
            })
            .collect(),
        Some(_) => Err(format!("{path}: \"{key}\" must be an array of strings")),
    }
}

/// `areev anonymize <set|list|clear|mappings>` — the per-namespace policy
/// surface (docs/anonymization-proposal.md P1). `scan` is handled pre-open.
fn run_anonymize(
    mut m: Areev,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub = positional.first().map(String::as_str).unwrap_or("");
    match sub {
        "set" => {
            let ns = flag(flags, "ns").ok_or("anonymize set requires --ns")?;
            let policy = match (flag(flags, "policy"), flag(flags, "policy-file")) {
                (Some(j), _) => j,
                (None, Some(path)) => std::fs::read_to_string(&path)
                    .map_err(|e| format!("--policy-file {path}: {e}"))?,
                (None, None) => return Err("anonymize set requires --policy or --policy-file".into()),
            };
            m.set_anon_policy(&ns, &policy).map_err(|e| e.to_string())?;
            println!("anonymization policy declared for namespace '{ns}' — egress reads are transformed from now on; older builds opening this file will warn");
            Ok(())
        }
        "list" => {
            let policies = m.anon_policies().map_err(|e| e.to_string())?;
            if policies.is_empty() {
                println!("no anonymization policies declared");
                return Ok(());
            }
            for (ns, p) in policies {
                println!(
                    "{}",
                    serde_json::json!({"ns": ns, "policy": p})
                );
            }
            Ok(())
        }
        "clear" => {
            let ns = flag(flags, "ns").ok_or("anonymize clear requires --ns")?;
            m.clear_anon_policy(&ns).map_err(|e| e.to_string())?;
            println!("anonymization policy cleared for namespace '{ns}'");
            Ok(())
        }
        "reveal" => {
            let ns = flag(flags, "ns").ok_or("anonymize reveal requires --ns")?;
            let token = flag(flags, "token").ok_or("anonymize reveal requires --token")?;
            // Facade path: admin-gated + Tier-2 audited (fingerprints only).
            let facade =
                areev_cal::AreevFacade::with_session(m, Some(ns.clone()), None);
            let out = facade
                .reveal_tokens(&ns, &[token])
                .map_err(|e| e.to_string())?;
            println!("{out}");
            Ok(())
        }
        "mappings" => {
            // In-process custody (D5): the operator running this process may
            // see the live placeholder->value mappings for rehydration.
            let maps = m.anon_mappings().map_err(|e| e.to_string())?;
            for (ns, id, mapping) in maps {
                println!(
                    "{}",
                    serde_json::json!({"ns": ns, "mapping_id": id, "mapping": mapping})
                );
            }
            Ok(())
        }
        other => Err(format!(
            "unknown anonymize subcommand '{other}' (expected scan, set, list, clear, or mappings)"
        )),
    }
}

fn run_retention(
    m: &mut Areev,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("list");
    match sub_cmd {
        "set" => {
            let days: f64 = flag(flags, "days")
                .and_then(|v| v.parse().ok())
                .or_else(|| positional.get(1).and_then(|v| v.parse().ok()))
                .ok_or_else(|| {
                    "usage: areev retention set --days N [--ns NS] [--type event] \
                     [--because \"why\"]"
                        .to_string()
                })?;
            let policy = areev_store::RetentionPolicy {
                days,
                grain_type: flag(flags, "type"),
                because: flag(flags, "because"),
            };
            m.set_retention_policy(ns, &policy).map_err(|e| e.to_string())?;
            println!(
                "retention policy for '{ns}': {days} days{}{}",
                policy.grain_type.map(|t| format!(", type {t}")).unwrap_or_default(),
                policy.because.map(|b| format!(" — {b}")).unwrap_or_default()
            );
            eprintln!(
                "areev: declared, not enforced — run `areev retention sweep --yes` (cron), \
                 or let the loop's retention_sweep analyzer propose it for review"
            );
        }
        "list" => {
            let policies = m.retention_policies().map_err(|e| e.to_string())?;
            if policies.is_empty() {
                println!("no retention policies declared in this memory");
            }
            for (pns, p) in policies {
                println!(
                    "{}",
                    serde_json::json!({
                        "namespace": pns,
                        "days": p.days,
                        "type": p.grain_type,
                        "because": p.because,
                    })
                );
            }
        }
        "clear" => {
            m.clear_retention_policy(ns).map_err(|e| e.to_string())?;
            println!("cleared the retention policy for '{ns}'");
        }
        "sweep" => {
            if flags.contains_key("no-destructive-ops") {
                return Err(
                    "destructive operations are disabled for this invocation \
                     (--no-destructive-ops)"
                        .into(),
                );
            }
            let policies = m.retention_policies().map_err(|e| e.to_string())?;
            if policies.is_empty() {
                println!("no retention policies declared — nothing to sweep");
                return Ok(());
            }
            if !flags.contains_key("yes") {
                let plan: Vec<String> = policies
                    .iter()
                    .map(|(pns, p)| {
                        format!(
                            "  {pns}: erase grains older than {} days{}",
                            p.days,
                            p.grain_type.as_deref().map(|t| format!(" (type {t})")).unwrap_or_default()
                        )
                    })
                    .collect();
                return Err(format!(
                    "retention sweep would apply {} policy/policies:\n{}\nRe-run with --yes \
                     to proceed.",
                    policies.len(),
                    plan.join("\n")
                ));
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let exports = m.corpus_exports().unwrap_or_else(|e| {
                eprintln!("areev: warning: could not consult corpus registry: {e}");
                Vec::new()
            });
            let results = m.sweep_retention(now).map_err(|e| e.to_string())?;
            let mut total = 0usize;
            for sweep in &results {
                let (pns, days) = (&sweep.ns, sweep.days);
                match &sweep.outcome {
                    areev_store::RetentionOutcome::Skipped { why } => {
                        // A held or floored namespace SKIPS with the refusal
                        // printed — never silently, never aborting the pass.
                        println!("{pns}: SKIPPED — {why}");
                        continue;
                    }
                    areev_store::RetentionOutcome::Swept(rep) => {
                        total += rep.grains_erased;
                        println!(
                            "{pns}: erased {} grains older than {days} days ({} vocabulary tokens, \
                             {} blobs reclaimed)",
                            rep.grains_erased, rep.vocab_removed, rep.blobs_reclaimed
                        );
                        // Audited like every other host-level erasure, one record per
                        // policy so the evidence names which rule fired — but only
                        // when something was actually erased. A destruction trail
                        // records destructions; a nightly no-op sweep would
                        // otherwise add 365 immutable, replicating grains a year per
                        // policy for zero information.
                        if rep.grains_erased > 0 {
                            let stale_exports = areev_store::exports_touching_hashes(
                                &exports,
                                &rep.erased_hashes,
                            );
                            write_erasure_audit(
                                m,
                                flags,
                                "erase",
                                &format!("retention:{days}d ns:{pns}"),
                                rep.grains_erased,
                                &stale_exports,
                            );
                        }
                    }
                }
            }
            println!("retention sweep: {total} grains erased across {} policies", results.len());
        }
        // The FLOOR half (governed-agents §5.4): destruction younger than
        // the floor refuses everywhere — CAL PURGE, sweeps, the loop.
        "floor" => {
            let min_days: f64 = flag(flags, "min-days")
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| {
                    "usage: areev retention floor --min-days N --because \"why\" [--ns NS]"
                        .to_string()
                })?;
            let because = flag(flags, "because").ok_or_else(|| {
                "retention floor requires --because (a floor with no recorded \
                 rationale is not auditable)"
                    .to_string()
            })?;
            m.set_retention_floor(ns, min_days, &because)
                .map_err(|e| e.to_string())?;
            println!("retention floor for '{ns}': ≥{min_days} days — {because}");
        }
        "floor-clear" => {
            m.clear_retention_floor(ns).map_err(|e| e.to_string())?;
            println!("retention floor cleared for '{ns}'");
        }
        "floors" => {
            let floors = m.retention_floors().map_err(|e| e.to_string())?;
            if floors.is_empty() {
                println!("no retention floors declared in this memory");
            }
            for (fns, days, because) in floors {
                println!("{fns}: ≥{days} days — {because}");
            }
        }
        other => {
            return Err(format!(
                "unknown retention subcommand '{other}' — usage: areev retention \
                 <set|list|clear|sweep|floor|floor-clear|floors>"
            ))
        }
    }
    Ok(())
}

/// `areev run` — the governed workflow runtime: start, resume, respond,
/// cancel, verify, fork, and the 10-minute proof (`demo`). Host tools
/// execute through `--tool-cmd` (input JSON on stdin, `AREEV_TOOL_NAME` /
/// `AREEV_IDEMPOTENCY_KEY` in the env, result JSON on stdout — the
/// `--embed-cmd` pattern); Client-bound nodes park the run and print the
/// `requires_action` envelope, answered with `areev run respond` by a
/// SECOND principal (approval separation of duties is refused structurally,
/// not documented). Wave 2: `--model provider:name` powers abstract nodes
/// (`--llm-max-tokens` is the per-turn ceiling AND the §6.7 reservation),
/// `--events` streams §6.10 run events to stderr as JSON lines, and
/// `fork --run-id BASE --as-run NEW [--at N] [--plan HASH]` is §5.4
/// time-travel / in-flight migration.
fn run_run(
    m: Areev,
    // The memory's own path, threaded in so the broker can serve this run's
    // CAS blobs to a declared capability tool (#106). Taken from the resolved
    // `--db` rather than from the open handle, because the lock-free blob read
    // deliberately works from the path and never the handle.
    db: &str,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    use areev_run::{RunSession, Runner, SystemClock};
    use std::sync::Arc;

    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("");
    let principal = flag(flags, "as").unwrap_or_else(|| "user:local".to_string());
    // The credential broker. A tool that posts to a vendor gets the broker's
    // address and a capability token -- never the token itself -- so the
    // secret stays in this process. `--tool-egress` is the per-tool grant:
    // which credentials a named tool may spend, and which methods it may
    // issue. A tool with no grant cannot even see the broker.
    // Held so the broker outlives the run; also the handle refusals are read
    // back from once it finishes.
    let mut broker_guard: Option<Arc<areev_run::Broker>> = None;
    let egress: Option<areev_run::EgressHandle> = match run_stack::build_egress(flags)? {
        None => None,
        Some(broker) => {
            // The other mediated door (#106): a module that declared
            // `{"blob": {"read": true}}` reads stored bytes through this same
            // broker, on this same token. Wired from the run's own memory
            // path, and by path rather than by handle — `read_blob_offline`
            // takes no lock, so serving an attachment never contends with the
            // driver's exclusive hold on the file it came from.
            broker.serve_blobs(db);
            let broker = Arc::new(broker);
            broker_guard = Some(Arc::clone(&broker));
            Some(areev_run::EgressHandle::new(broker))
        }
    };
    // The executor stack, the model and the ceilings all come from
    // `run_stack` — the one builder `trigger run` calls too, so a plan that
    // executes here executes there (#90).
    let executor = run_stack::tool_executor(flags, egress.as_ref());
    let llm = run_stack::toolcall_llm(flags)?;
    let observer = run_stack::observer(flags)?;
    let runner = Runner {
        facade: Arc::new(AreevFacade::with_session(m, Some(ns.to_string()), None)),
        clock: Arc::new(SystemClock),
        executor,
        llm,
        observer,
        ns: ns.to_string(),
        principal: principal.clone(),
    };
    let opts = run_stack::run_options(flags);

    let need = |key: &str, usage: &str| -> Result<String, String> {
        flag(flags, key).ok_or_else(|| format!("usage: {usage}"))
    };
    let print_session = |session: RunSession| match session {
        RunSession::Finished { outcome, run_id } => {
            println!(
                "{}",
                serde_json::json!({"run_id": run_id, "finished": format!("{outcome:?}")})
            );
        }
        RunSession::Parked { envelope, .. } => {
            println!("{envelope}");
            eprintln!(
                "areev: run parked — answer with `areev run respond --run-id ID \
                 --ask TOOL_CALL_ID --result JSON --as PRINCIPAL`, then \
                 `areev run resume --run-id ID`"
            );
        }
    };

    let report_refusals = run_stack::report_refusals;
    match sub_cmd {
        "start" => {
            let wf = need("workflow", "areev run start --workflow HASH --run-id ID [--input JSON] [--tool-cmd CMD] [--as PRINCIPAL]")?;
            let run_id = need("run-id", "areev run start --workflow HASH --run-id ID")?;
            let input: serde_json::Value = match flag(flags, "input") {
                Some(raw) => serde_json::from_str(&raw)
                    .map_err(|e| format!("--input must be valid JSON: {e}"))?,
                None => serde_json::json!({}),
            };
            let h = Hash::from_hex(&wf).map_err(|e| e.to_string())?;
            let session = runner.start(&h, &run_id, input, &opts).map_err(|e| e.to_string())?;
            print_session(session);
        }
        "resume" => {
            let run_id = need("run-id", "areev run resume --run-id ID [--tool-cmd CMD]")?;
            let session = runner.resume(&run_id, &opts).map_err(|e| e.to_string())?;
            print_session(session);
        }
        "respond" => {
            let run_id = need("run-id", "areev run respond --run-id ID --ask TOOL_CALL_ID --result JSON --as PRINCIPAL")?;
            let ask = need("ask", "areev run respond --run-id ID --ask TOOL_CALL_ID --result JSON --as PRINCIPAL")?;
            let result: serde_json::Value = serde_json::from_str(
                &need("result", "areev run respond … --result JSON")?,
            )
            .map_err(|e| format!("--result must be valid JSON: {e}"))?;
            let is_error = flag(flags, "is-error")
                .map(|v| !matches!(v.as_str(), "false" | "0" | "off" | "no"))
                .unwrap_or(false);
            runner
                .respond(&run_id, &ask, result, is_error, &principal)
                .map_err(|e| e.to_string())?;
            println!("response recorded — `areev run resume --run-id {run_id}` to continue");
        }
        "cancel" => {
            let run_id = need("run-id", "areev run cancel --run-id ID [--because \"why\"]")?;
            let because = flag(flags, "because").unwrap_or_else(|| "canceled".into());
            runner
                .cancel(&run_id, &principal, &because)
                .map_err(|e| e.to_string())?;
            println!(
                "cancel recorded for '{run_id}' — a live driver drains at its next \
                 superstep boundary; `areev run resume --run-id {run_id}` finalizes a \
                 parked one"
            );
        }
        "verify" => {
            let run_id = need("run-id", "areev run verify --run-id ID")?;
            let report = runner.verify(&run_id).map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            if !report.verified {
                return Err("verification did not pass".into());
            }
        }
        "list" => {
            // Recent runs, newest first, with terminal outcome + spend when
            // recorded (the run-outcome Observation) — "what has this
            // memory been running?" as one command.
            let n = flag(flags, "last").and_then(|v| v.parse().ok()).unwrap_or(20);
            let ids = runner.recent_runs(n).map_err(|e| e.to_string())?;
            let mut rows = Vec::new();
            for id in &ids {
                let outcome = runner.facade.with_store(|m| {
                    m.latest("agent:harness", &format!("run:{id}"), "mg:harness").ok().flatten()
                });
                let obs = runner.facade.with_store(|m| {
                    m.recent("agent:harness", Some(areev_core::types::GrainType::Observation), 4096)
                }).ok().and_then(|rows| {
                    rows.into_iter().find(|g| {
                        g.get_str("run_id") == Some(id.as_str())
                            && g.get_str("observation_kind") == Some("run_outcome")
                    })
                });
                rows.push(serde_json::json!({
                    "run_id": id,
                    "outcome": obs.as_ref().and_then(|g| g.get_str("object")).unwrap_or("open"),
                    "usd_micros": obs.as_ref().and_then(|g| g.get_u64("spent_usd_micros")),
                    "tokens": obs.as_ref().map(|g| {
                        g.get_u64("spent_input_tokens").unwrap_or(0)
                            + g.get_u64("spent_output_tokens").unwrap_or(0)
                    }),
                    "has_manifest": outcome.is_some(),
                }));
            }
            println!("{}", serde_json::to_string_pretty(&rows).unwrap());
        }
        "inspect" => {
            // One run, whole picture: the frozen manifest, terminal outcome,
            // spend, journal size, pending asks, fork lineage — a thin
            // wrapper over Runner::inspect (also reachable in-process from
            // the Python/Node bindings, issue #34).
            let run_id = need("run-id", "areev run inspect --run-id ID")?;
            let report = runner.inspect(&run_id).map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        }
        "oversight-report" => {
            // The Art. 14 row as a command (docs/eu-ai-act.md): where can a
            // human intervene, who is AUTHORIZED to, what expires when, and
            // how fast the kill switch actually drained — measured from the
            // journal, not asserted. A thin wrapper over
            // Runner::oversight_report (also reachable in-process from the
            // Python/Node bindings, issue #34).
            let plan_hash = flag(flags, "plan")
                .map(|p| {
                    areev_core::error::Hash::from_hex(&p)
                        .map_err(|_| format!("--plan is not a valid hash: {p}"))
                })
                .transpose()?;
            let report = runner
                .oversight_report(flag(flags, "run-id").as_deref(), plan_hash.as_ref())
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        }
        "shadow" => {
            // §7.4's pre-apply check as a product surface: replay journaled
            // runs with zero effect dispatches (structural — the verify path
            // has no executor) and report per-run consistency.
            let ids: Vec<String> = match flag(flags, "runs") {
                Some(csv) => csv.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                None => {
                    let n = flag(flags, "last").and_then(|v| v.parse().ok()).unwrap_or(5);
                    runner.recent_runs(n).map_err(|e| e.to_string())?
                }
            };
            if ids.is_empty() {
                return Err("no runs to shadow (give --runs a,b or record some runs first)".into());
            }
            let report = runner.shadow_eval(&ids);
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            if !report.all_consistent {
                return Err("shadow evaluation found inconsistent runs".into());
            }
        }
        "fork" => {
            let usage = "areev run fork --run-id BASE --as-run NEW [--at SUPERSTEP] [--plan HASH]";
            let base = need("run-id", usage)?;
            let new_id = need("as-run", usage)?;
            let at = flag(flags, "at").and_then(|v| v.parse().ok());
            let plan = match flag(flags, "plan") {
                Some(h) => Some(Hash::from_hex(&h).map_err(|e| e.to_string())?),
                None => None,
            };
            let seed = runner
                .fork(&base, at, &new_id, plan.as_ref(), &opts)
                .map_err(|e| e.to_string())?;
            println!(
                "fork '{new_id}' created from '{base}' (seed checkpoint {seed}) — \
                 `areev run resume --run-id {new_id}` continues it"
            );
        }
        // The 10-minute proof: seed a two-node plan (host tool + human
        // approval) and print the exact sequence to run it, pause it,
        // approve it as a second principal, resume it, and verify it.
        "demo" => {
            let (wf_hash, greet_hash, approve_hash) = runner.facade.with_store(|m| {
                let greet = areev_core::types::Tool::new("greet")
                    .kind(areev_core::types::ToolKind::Definition)
                    .tool_description("Greets the subject (host-executed)")
                    .created_at(1_000)
                    .namespace(ns);
                let gh = m.add(&greet)?;
                let approve = areev_core::types::Tool::new("approve")
                    .kind(areev_core::types::ToolKind::Definition)
                    .tool_description("A human approves the greeting")
                    .executor_kind(areev_core::types::ExecutorKind::Client)
                    .created_at(1_001)
                    .namespace(ns);
                let ah = m.add(&approve)?;
                let wf = areev_core::types::Workflow::new(vec!["greet".into(), "approve".into()])
                    .edge("greet", "approve")
                    .bind("greet", &gh.to_hex())
                    .bind("approve", &ah.to_hex())
                    .created_at(1_002)
                    .namespace(ns);
                let wh = m.add(&wf)?;
                Ok::<_, areev_core::error::AreevError>((wh, gh, ah))
            })
            .map_err(|e| e.to_string())?;
            println!("demo plan seeded (workflow {wf_hash})");
            println!("  greet   -> host tool {greet_hash}");
            println!("  approve -> human approval {approve_hash}");
            println!();
            println!("the 10-minute proof (no LLM key required):");
            println!(
                "  1. areev run start --db <DB> --workflow {wf_hash} --run-id demo-1 \\\n\
                 \x20      --input '{{\"who\":\"world\"}}' --tool-cmd 'printf '\\''{{\"greeting\":\"hello\"}}'\\'''"
            );
            println!("     → parks with a requires_action envelope; copy its tool_call_id");
            println!(
                "  2. areev run respond --db <DB> --run-id demo-1 --ask <TOOL_CALL_ID> \\\n\
                 \x20      --result '{{\"approved\":true}}' --as user:officer"
            );
            println!("     (responding as the starter is REFUSED — separation of duties)");
            println!("  3. areev run resume --db <DB> --run-id demo-1  → Completed");
            println!("  4. areev run verify --db <DB> --run-id demo-1  → journal-consistent replay");
            println!("  5. areev run-trace --db <DB> --run-id demo-1   → the full journal");
        }
        other => {
            return Err(format!(
                "unknown run subcommand '{other}' — usage: areev run \
                 <start|resume|respond|cancel|list|inspect|verify|fork|shadow|oversight-report|demo>"
            ))
        }
    }
    report_refusals(&broker_guard);
    Ok(())
}

/// `areev tool provenance <hash>` (§7.4): the full chain for one piece of
/// governed code, as ONE command — the recommendation(s) targeting it, each
/// lifecycle transition with approver + BECAUSE, the recorded gating edge
/// (evalset hash, gate run, stats), and which runs the code touched since.
/// One-command forensics is a product surface, not an implied join.
fn run_tool_provenance(
    mut m: Areev,
    ns: &str,
    hash_arg: &str,
    flags: &HashMap<String, String>,
) -> Result<(), String> {
    use areev_loop::SubstrateRead;
    let depth = flag(flags, "depth").and_then(|v| v.parse().ok()).unwrap_or(2);
    // Store-level joins FIRST (the substrate takes ownership below).
    let runs = match Hash::from_hex(hash_arg) {
        Ok(h) => {
            let mut all = m.runs_touching(ns, &h, depth).unwrap_or_default();
            if ns != "agent:harness" {
                all.extend(m.runs_touching("agent:harness", &h, depth).unwrap_or_default());
            }
            all.sort();
            all.dedup();
            all
        }
        Err(_) => Vec::new(),
    };
    let sub = AreevSubstrate::new(m, None);
    let engine = Engine::with_builtins();
    let target = format!("tool:{hash_arg}");
    let recs: Vec<_> = engine
        .recommendations(&sub, None)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|r| r.target_ref == target)
        .collect();
    // Audit chain per recommendation, oldest first (loop_audit observations).
    let audits = sub
        .grains_of_type("observation", Some("areev-loop"), Default::default())
        .unwrap_or_default();
    let mut out_recs = Vec::new();
    for r in &recs {
        let mut chain: Vec<serde_json::Value> = audits
            .iter()
            .filter(|a| a.str_field("rec_hash") == Some(r.hash.as_str()))
            .map(|a| {
                let mut row = serde_json::json!({
                    "to": a.str_field("to_status"),
                    "actor": a.str_field("actor"),
                    "because": a.str_field("because"),
                    "at_ms": a.fields.get("at_ms"),
                });
                if let Some(es) = a.str_field("gating_evalset") {
                    row["gating"] = serde_json::json!({
                        "evalset": es,
                        "run_id": a.str_field("gating_run_id"),
                        "passed": a.fields.get("gating_passed"),
                        "failed": a.fields.get("gating_failed"),
                    });
                }
                row
            })
            .collect();
        chain.sort_by_key(|c| c.get("at_ms").and_then(|v| v.as_i64()).unwrap_or(0));
        out_recs.push(serde_json::json!({
            "recommendation": r.hash,
            "status": r.status.as_str(),
            "summary": r.summary.render(),
            "evalset_pin": r.evalset_hash,
            "lifecycle": chain,
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "tool": hash_arg,
            "recommendations": out_recs,
            "runs_touching": runs,
            "runs_touching_count": runs.len(),
        }))
        .unwrap()
    );
    if out_recs.is_empty() && runs.is_empty() {
        eprintln!("note: nothing recorded for this hash yet (no recommendations target it; no runs touch it)");
    }
    Ok(())
}

/// `areev eval` — evalsets and the §7.4 gating edge.
///
/// - `create --name N --cases FILE` stores an evalset grain (cases as
///   content: `[{"name", "input", "expect": {"equals": …}|{"contains": …}}]`)
///   and prints its hash — the hash a `code_revision` recommendation pins.
/// - `run --evalset HASH --tool-cmd CMD` executes every case through the
///   subprocess seam (case input on stdin, result JSON on stdout,
///   `AREEV_EVAL_CASE`/`AREEV_EVALSET` in the env), journals each case as a
///   Tool grain under a fresh `eval-…` run id, writes the summary Fact
///   (`evalset:<hash> mg:eval_run {run_id, passed, failed}`), and prints the
///   stats. That summary IS the gating evidence `areev loop apply
///   --gating-run` loads — apply of a code revision without it is refused.
fn run_eval(
    m: Areev,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    const EVAL_NS: &str = "agent:harness";
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("");
    let facade = AreevFacade::with_session(m, Some(EVAL_NS.to_string()), None);
    let need = |key: &str, usage: &str| -> Result<String, String> {
        flag(flags, key).ok_or_else(|| format!("usage: {usage}"))
    };
    match sub_cmd {
        "create" => {
            let usage = "areev eval create --name NAME --cases FILE";
            let name = need("name", usage)?;
            let path = need("cases", usage)?;
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
            let cases: Vec<serde_json::Value> =
                serde_json::from_str(&text).map_err(|e| format!("cases must be a JSON list: {e}"))?;
            if cases.is_empty() {
                return Err("an evalset needs at least one case".into());
            }
            for (i, c) in cases.iter().enumerate() {
                let ok = c.get("name").and_then(|v| v.as_str()).is_some()
                    && c.get("input").is_some()
                    && c.get("expect").is_some_and(|e| {
                        e.get("equals").is_some() || e.get("contains").and_then(|v| v.as_str()).is_some()
                    });
                if !ok {
                    return Err(format!(
                        "case {i} must be {{\"name\", \"input\", \"expect\": {{\"equals\": …}} | {{\"contains\": \"…\"}}}}"
                    ));
                }
            }
            let payload = serde_json::json!({"name": name, "cases": cases});
            let mut fact = areev_core::types::Fact::new(
                &format!("evalset:{name}"),
                "mg:evalset",
                &payload.to_string(),
            )
            .namespace(EVAL_NS);
            fact.common.extra_fields.insert("evalset_name".into(), serde_json::json!(name));
            let h = facade.with_store(|m| m.add(&fact)).map_err(|e| e.to_string())?;
            println!("evalset '{name}' stored: {h} ({} cases)", cases.len());
            Ok(())
        }
        "run" => {
            let usage = "areev eval run --evalset HASH (--tool-cmd CMD | --model provider:name \
                         [--base-url URL] [--key-env VAR] [--llm-max-tokens N])";
            let evalset_hex = need("evalset", usage)?;
            // Exactly one executor: a host command, or a model behind the
            // ToolCallLlm seam (how a tuned adapter served by vLLM / SGLang /
            // Ollama is graded — `--model openai-compat:<served-name>`).
            let cmd = flag(flags, "tool-cmd");
            let model = flag(flags, "model");
            let llm = match (&cmd, &model) {
                (Some(_), Some(_)) => {
                    return Err("give either --tool-cmd or --model, not both".into());
                }
                (None, None) => return Err(usage.to_string()),
                (Some(_), None) => None,
                (None, Some(spec)) => Some(
                    areev_llm::resolve_toolcall(
                        spec,
                        flag(flags, "base-url").as_deref(),
                        flag(flags, "key-env").as_deref(),
                    )
                    .map_err(|e| e.to_string())?,
                ),
            };
            let llm_max_tokens: u32 = flag(flags, "llm-max-tokens")
                .map(|v| v.parse().map_err(|_| "--llm-max-tokens must be a number".to_string()))
                .transpose()?
                .unwrap_or(1024);
            let h = Hash::from_hex(&evalset_hex).map_err(|e| e.to_string())?;
            let grain = facade.with_store(|m| m.get(&h)).map_err(|e| e.to_string())?;
            let payload: serde_json::Value = grain
                .get_str("object")
                .and_then(|o| serde_json::from_str(o).ok())
                .ok_or("grain is not an evalset (no JSON object payload)")?;
            let cases = payload
                .get("cases")
                .and_then(|c| c.as_array())
                .cloned()
                .ok_or("evalset payload carries no cases")?;
            let run_id = format!(
                "eval-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            // Fail-closed: a gate run that cannot run EVERY case records
            // nothing. The model path constrains case-input shapes (a prompt
            // string or a messages array); tool-cmd cases stay any-JSON.
            if llm.is_some() {
                for case in &cases {
                    let cname = case.get("name").and_then(|v| v.as_str()).unwrap_or("case");
                    let input = case.get("input").cloned().unwrap_or(serde_json::json!({}));
                    eval_model_messages(&input).map_err(|e| format!("case '{cname}': {e}"))?;
                }
            }
            let (mut passed, mut failed) = (0u64, 0u64);
            let mut rows = Vec::new();
            for case in &cases {
                let cname = case.get("name").and_then(|v| v.as_str()).unwrap_or("case");
                let input = case.get("input").cloned().unwrap_or(serde_json::json!({}));
                let expect = case.get("expect").cloned().unwrap_or(serde_json::json!({}));
                let (ok, got) = match &llm {
                    Some(llm) => {
                        run_eval_case_model(llm.as_ref(), &input, &expect, llm_max_tokens)
                    }
                    None => run_eval_case(
                        cmd.as_deref().unwrap_or_default(),
                        &evalset_hex,
                        cname,
                        &input,
                        &expect,
                    ),
                };
                if ok { passed += 1 } else { failed += 1 };
                // Each case is journaled — the gate run is inspectable with
                // `areev run-trace --run-id <eval id>` like any other run.
                facade
                    .record_tool_call(
                        EVAL_NS,
                        &format!("eval:{cname}"),
                        Some(&input.to_string()),
                        &got,
                        !ok,
                        None,
                        None,
                        Some(&run_id),
                        None,
                        None,
                        Some(if ok { "completed" } else { "failed" }),
                        None,
                        Some("host"),
                        None,
                    )
                    .map_err(|e| e.to_string())?;
                rows.push(serde_json::json!({"case": cname, "ok": ok}));
            }
            // The summary Fact — the RECORDED edge apply-gating loads. The
            // grading model rides beside the stats: for an adapter gate,
            // WHICH served model was judged is part of the evidence.
            let mut summary = serde_json::json!({
                "run_id": run_id, "passed": passed, "failed": failed,
            });
            if let Some(spec) = &model {
                summary["model"] = serde_json::json!(spec);
            }
            let mut fact = areev_core::types::Fact::new(
                &format!("evalset:{evalset_hex}"),
                "mg:eval_run",
                &summary.to_string(),
            )
            .namespace(EVAL_NS);
            fact.common.extra_fields.insert("run_id".into(), serde_json::json!(run_id));
            facade.with_store(|m| m.add(&fact)).map_err(|e| e.to_string())?;

            // Re-acceptance (the enterprise plane's model-swap harness):
            // --baseline RUN_ID compares this run's pass rate against a
            // RECORDED earlier gate run of the same evalset, within
            // --tolerance percentage points (default 0 — any drop fails).
            // The comparison itself is recorded as a Fact, so "we re-ran
            // acceptance after the model swap" is evidence, not a claim.
            let mut reacceptance = serde_json::Value::Null;
            if let Some(baseline_run) = flag(flags, "baseline") {
                let tolerance: f64 = flag(flags, "tolerance")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                let baseline = json_from_facade_recall(&facade, EVAL_NS, &format!("evalset:{evalset_hex}"))?
                    .into_iter()
                    .filter(|row| row["fields"]["relation"] == "mg:eval_run")
                    .filter_map(|row| {
                        serde_json::from_str::<serde_json::Value>(row["fields"]["object"].as_str()?)
                            .ok()
                    })
                    .find(|s| s["run_id"] == baseline_run.as_str())
                    .ok_or_else(|| {
                        format!("no recorded gate run '{baseline_run}' for this evalset")
                    })?;
                let rate = |p: u64, f: u64| {
                    if p + f == 0 { 0.0 } else { p as f64 / (p + f) as f64 * 100.0 }
                };
                let base_rate = rate(
                    baseline["passed"].as_u64().unwrap_or(0),
                    baseline["failed"].as_u64().unwrap_or(0),
                );
                let this_rate = rate(passed, failed);
                let within = this_rate + 1e-9 >= base_rate - tolerance;
                reacceptance = serde_json::json!({
                    "baseline_run": baseline_run,
                    "baseline_pass_rate": base_rate,
                    "pass_rate": this_rate,
                    "tolerance_points": tolerance,
                    "accepted": within,
                });
                let mut cmp = areev_core::types::Fact::new(
                    &format!("evalset:{evalset_hex}"),
                    "mg:reacceptance",
                    &reacceptance.to_string(),
                )
                .namespace(EVAL_NS);
                cmp.common.extra_fields.insert("run_id".into(), serde_json::json!(run_id));
                facade.with_store(|m| m.add(&cmp)).map_err(|e| e.to_string())?;
                if !within {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "evalset": evalset_hex, "run_id": run_id,
                            "passed": passed, "failed": failed,
                            "reacceptance": reacceptance,
                        }))
                        .unwrap()
                    );
                    return Err(format!(
                        "re-acceptance FAILED: pass rate {this_rate:.1}% vs baseline \
                         {base_rate:.1}% (tolerance {tolerance} points)"
                    ));
                }
            }

            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "evalset": evalset_hex, "run_id": run_id,
                    "passed": passed, "failed": failed, "cases": rows,
                    "reacceptance": reacceptance,
                }))
                .unwrap()
            );
            if failed > 0 {
                return Err(format!("{failed} case(s) failed"));
            }
            Ok(())
        }
        other => Err(format!(
            "unknown eval subcommand '{other}' — usage: areev eval <create|run>"
        )),
    }
}

/// Recall a subject's rows through a facade as parsed JSON.
fn json_from_facade_recall(
    facade: &AreevFacade,
    ns: &str,
    subject: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = facade
        .with_store(|m| m.recall(ns, subject, None, 100_000))
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|g| serde_json::json!({"fields": g.fields, "hash": g.hash.to_hex()}))
        .collect())
}

/// One eval case through the subprocess seam. Returns (passed, output text).
fn run_eval_case(
    cmd: &str,
    evalset: &str,
    case: &str,
    input: &serde_json::Value,
    expect: &serde_json::Value,
) -> (bool, String) {
    use areev_core::proc::{self, SpawnPolicy};
    use std::process::Command;
    // The platform shell: /bin/sh -c on unix, cmd /C on Windows. The
    // Windows command string must go through raw_arg — Command::arg
    // MSVC-quotes embedded quotes, which cmd.exe does not parse.
    #[cfg(not(windows))]
    let mut shell = Command::new("/bin/sh");
    #[cfg(not(windows))]
    shell.arg("-c").arg(cmd);
    #[cfg(windows)]
    let mut shell = Command::new("cmd");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        shell.raw_arg("/C").raw_arg(cmd);
    }
    let out = match proc::run(
        shell,
        Some(input.to_string().as_bytes()),
        &[("AREEV_EVALSET", evalset), ("AREEV_EVAL_CASE", case)],
        &SpawnPolicy::default(),
    ) {
        Ok(o) => o,
        Err(e) => return (false, format!("spawn failed: {e}")),
    };
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if let Some(why) = out.failure("eval case") {
        return (false, why);
    }
    (eval_expect_matches(expect, &stdout), stdout)
}

/// The ONE scorer, shared by the tool-cmd and model paths: `expect.equals`
/// compares parsed JSON, `expect.contains` is a substring test, anything
/// else fails. Two scorers would let the two paths drift on what "passed"
/// means — the gate's whole job.
fn eval_expect_matches(expect: &serde_json::Value, got: &str) -> bool {
    if let Some(eq) = expect.get("equals") {
        serde_json::from_str::<serde_json::Value>(got)
            .map(|parsed| &parsed == eq)
            .unwrap_or(false)
    } else if let Some(needle) = expect.get("contains").and_then(|v| v.as_str()) {
        got.contains(needle)
    } else {
        false
    }
}

/// Map a model-path case `input` onto chat messages: a string is one user
/// message; an array of `{role, content}` (`system` | `user` | `assistant`)
/// is a conversation, with at most one leading `system` lifted into the
/// request's system slot. Anything else is refused — BEFORE any case runs.
fn eval_model_messages(
    input: &serde_json::Value,
) -> Result<(Option<String>, Vec<areev_llm::ChatMessage>), String> {
    use areev_llm::ChatMessage;
    if let Some(prompt) = input.as_str() {
        return Ok((None, vec![ChatMessage::User(prompt.to_string())]));
    }
    let Some(turns) = input.as_array() else {
        return Err(
            "--model cases need a string prompt or an array of {role, content} \
             messages (tool-cmd cases take any JSON)"
                .into(),
        );
    };
    let mut system = None;
    let mut messages = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        let role = turn.get("role").and_then(|r| r.as_str()).unwrap_or_default();
        let content = turn
            .get("content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| format!("message {i} has no string content"))?;
        match role {
            "system" if i == 0 => system = Some(content.to_string()),
            "system" => return Err(format!("message {i}: system must be the first message")),
            "user" => messages.push(ChatMessage::User(content.to_string())),
            "assistant" => messages.push(ChatMessage::Assistant {
                text: Some(content.to_string()),
                tool_calls: vec![],
            }),
            other => return Err(format!("message {i} has unsupported role {other:?}")),
        }
    }
    if messages.is_empty() {
        return Err("a --model case needs at least one user or assistant message".into());
    }
    Ok((system, messages))
}

/// Run one case against the model behind the ToolCallLlm seam: no tools,
/// temperature 0.0, the response text scored by the same matcher as a
/// tool-cmd case's stdout.
fn run_eval_case_model(
    llm: &dyn areev_llm::ToolCallLlm,
    input: &serde_json::Value,
    expect: &serde_json::Value,
    max_tokens: u32,
) -> (bool, String) {
    use areev_llm::{ToolCallRequest, ToolChoice};
    // Prevalidated before the run started; a failure here is still a case
    // failure rather than a panic.
    let (system, messages) = match eval_model_messages(input) {
        Ok(v) => v,
        Err(e) => return (false, e),
    };
    let req = ToolCallRequest {
        system,
        messages,
        tools: &[],
        tool_choice: ToolChoice::None,
        max_tokens,
        temperature: 0.0,
    };
    match llm.call(&req) {
        Ok(resp) => {
            let text = resp.text.unwrap_or_default().trim().to_string();
            (eval_expect_matches(expect, &text), text)
        }
        Err(e) => (false, format!("model call failed: {}", e.message)),
    }
}

/// `areev hold` — legal holds (governed-agents §5.4): while a hold is live on
/// a namespace, ALL age-based destruction there refuses; sweeps skip it
/// with the refusal on record. Erasure-vs-hold precedence (D10's
/// `--override-hold` on FORGET SUBJECT) ships with the compliance wave.
fn run_hold(
    m: &mut Areev,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("list");
    match sub_cmd {
        "set" => {
            let because = flag(flags, "because").ok_or_else(|| {
                "usage: areev hold set --because \"litigation X\" [--ns NS] [--by PRINCIPAL]"
                    .to_string()
            })?;
            let by = flag(flags, "by").unwrap_or_else(|| "user:local".into());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            m.place_hold(ns, &because, &by, now).map_err(|e| e.to_string())?;
            println!("hold placed on '{ns}' by {by}: {because}");
        }
        "release" => {
            m.release_hold(ns).map_err(|e| e.to_string())?;
            println!("hold released on '{ns}'");
        }
        "list" => {
            let holds = m.holds().map_err(|e| e.to_string())?;
            if holds.is_empty() {
                println!("no legal holds in this memory");
            }
            for (hns, because, by) in holds {
                println!("{hns}: held by {by} — {because}");
            }
        }
        other => {
            return Err(format!(
                "unknown hold subcommand '{other}' — usage: areev hold <set|release|list>"
            ))
        }
    }
    Ok(())
}

/// Read a stream dir's `CURSOR`: `"<gen_id> <op_seq> <next_seg>"`.
///
/// The third field is what keeps retention safe. Dirs written before it
/// existed carry only two fields; for those we fall back to counting
/// segment files, which is correct precisely because such a dir has never
/// been pruned (counting is what pruning breaks).
fn read_stream_cursor(cursor_path: &str, to: &str) -> (String, i64, u32) {
    let Ok(s) = std::fs::read_to_string(cursor_path) else {
        return (String::new(), 0, 0);
    };
    let mut parts = s.trim().splitn(3, ' ');
    let gen_id = parts.next().unwrap_or("").to_string();
    let cursor: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let seg = match parts.next().and_then(|v| v.parse::<u32>().ok()) {
        Some(n) => n,
        None => std::fs::read_dir(format!("{to}/gen-{gen_id}"))
            .map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|x| x == "mgb")).count() as u32)
            .unwrap_or(0),
    };
    (gen_id, cursor, seg)
}

/// Drop whole generations older than `window_ms`, never individual segments
/// inside one — a hole in a live generation strands every follower at the
/// gap, while dropping a whole generation is a re-baseline the follower
/// handles. The current generation is always kept.
///
/// This is what makes the Art. 17 archive guarantee true: each generation's
/// segment 0 is a snapshot of the already-erased store, so once the
/// pre-erasure generations age out, the erased bytes are gone from the
/// archive as well as the live store. See docs/gdpr.md §3.
fn prune_generations(to: &str, current_gen: &str, window_ms: i64) -> Result<usize, String> {
    let now = std::time::SystemTime::now();
    let mut dropped = 0usize;
    for entry in std::fs::read_dir(to).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !path.is_dir() || !name.starts_with("gen-") || name == format!("gen-{current_gen}") {
            continue;
        }
        // Age by the NEWEST segment in the generation: a generation is only
        // expendable once everything in it has aged out.
        let newest = std::fs::read_dir(&path)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|f| f.metadata().ok().and_then(|md| md.modified().ok()))
            .max();
        let Some(newest) = newest else { continue };
        let age_ms = now.duration_since(newest).map(|d| d.as_millis() as i64).unwrap_or(0);
        if age_ms > window_ms {
            std::fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
            dropped += 1;
        }
    }
    Ok(dropped)
}

/// Parse a duration like `6h` / `30m` / `2d` / `3600s` into milliseconds.
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = s[..split].parse().ok()?;
    let mult = match &s[split..] {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(n * mult)
}

fn parse_severity(s: &str) -> Severity {
    match s.to_ascii_lowercase().as_str() {
        "high" => Severity::High,
        "medium" => Severity::Medium,
        "low" => Severity::Low,
        _ => Severity::Info,
    }
}

/// `--status` filter; default is `pending`, `all` clears the filter.
fn status_filter(flags: &HashMap<String, String>) -> Option<RecStatus> {
    match flag(flags, "status").as_deref() {
        Some("approved") => Some(RecStatus::Approved),
        Some("rejected") => Some(RecStatus::Rejected),
        Some("applied") => Some(RecStatus::Applied),
        Some("rolled_back") => Some(RecStatus::RolledBack),
        Some("expired") => Some(RecStatus::Expired),
        Some("all") => None,
        _ => Some(RecStatus::Pending),
    }
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Resolve a git-style unique hash prefix to a full recommendation hash.
/// Reconstruct the §7.4 gating evidence from the RECORDED gate-run summary
/// (`evalset:<pin> mg:eval_run` Fact in `agent:harness`) — the operator
/// names the run; the numbers come from what `areev eval run` wrote, never
/// from the command line.
fn load_gating_evidence(
    sub: &AreevSubstrate,
    engine: &Engine,
    rec_hash: &str,
    run_id: &str,
) -> Result<areev_loop::GatingEvidence, String> {
    // The ONE loader every surface shares (`Engine::gating_evidence`): the
    // stats that admit a gated revision are read back from the recorded
    // `mg:eval_run` Fact, never taken from the caller.
    engine.gating_evidence(sub, rec_hash, run_id).map_err(|e| e.to_string())
}

fn resolve_hash(engine: &Engine, sub: &AreevSubstrate, prefix: &str) -> Result<String, String> {
    let recs = engine.recommendations(sub, None).map_err(|e| e.to_string())?;
    let matches: Vec<&str> = recs
        .iter()
        .map(|r| r.hash.as_str())
        .filter(|h| h.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(format!("no recommendation matches '{prefix}'")),
        1 => Ok(matches[0].to_string()),
        n => Err(format!("'{prefix}' is ambiguous ({n} matches) — use more characters")),
    }
}

/// Resolve an LLM backend from a `--<prefix>cmd` / `--<prefix>model` flag pair,
/// the same two ways `areev loop run` attaches one: a subprocess (the
/// zero-dependency escape hatch) or a built-in HTTP provider whose key is read
/// from the environment, never from the command line. The subprocess wins when
/// both are given. Both fail at construction, before anything is written.
fn resolve_llm(
    flags: &HashMap<String, String>,
    cmd_flag: &str,
    model_flag: &str,
) -> Result<Option<Box<dyn areev_loop::LlmBackend>>, String> {
    if let Some(cmd) = flag(flags, cmd_flag) {
        let label = flag(flags, "llm-model");
        let llm = areev_loop::CommandLlm::new(&cmd, label.as_deref()).map_err(|e| e.to_string())?;
        return Ok(Some(Box::new(llm)));
    }
    if let Some(spec) = flag(flags, model_flag) {
        let base = flag(flags, "llm-base-url");
        let key_env = flag(flags, "llm-api-key-env");
        let llm = areev_llm::resolve(&spec, base.as_deref(), key_env.as_deref())
            .map_err(|e| e.to_string())?;
        return Ok(Some(llm));
    }
    Ok(None)
}

/// `areev remember` — store free text as an Event, then attach the facts
/// distilled from it.
///
/// Three routes to those facts, in precedence order:
///
/// - `--facts JSON` — the host already extracted them; no model is called and
///   the facts carry no model attribution (the host is asserting them).
/// - `--llm-cmd CMD` — a subprocess backend.
/// - `--model provider:name` — a built-in HTTP backend, key from the env.
///
/// The raw text is written **before** the model is called, so a failed or
/// garbage extraction never costs the raw text. Model-extracted facts are
/// stamped `verification_status = "unverified"` (CAL-filterable, so they can be
/// reviewed) unless `--ground-model`/`--ground-cmd` runs a separate entailment
/// pass over them — a different call from the one that proposed them, the same
/// proposer ≠ scorer rule the Areev Loop verifier follows. Facts the grounder does
/// not support are dropped; survivors are stamped `"verified"`.
fn run_remember(mut m: Areev, ns: &str, flags: &HashMap<String, String>) -> Result<(), String> {
    let mut content = need(flags, "content")?;
    // Anonymization boundaries (docs/anonymization-proposal.md §4.1–4.2):
    // under an INGRESS policy, transform before the extractor OR the store
    // sees the text — the CLI composes capture+attach itself, so it must not
    // hand the raw text to the model first. Under an EGRESS policy the
    // extraction request is additionally covered below by the
    // PseudonymizingBackend wrap, so identities stay in-process even when
    // the stored form is raw.
    if let Some(t) = m.ingress_transform_text(ns, &content).map_err(|e| e.to_string())? {
        content = t;
    }
    let egress_wrap = matches!(
        m.anon_active_mode(ns).map_err(|e| e.to_string())?.as_deref(),
        Some("egress")
    );
    let observer = flag(flags, "observer").unwrap_or_else(|| "cli".to_string());
    let dry_run = flag(flags, "dry-run").is_some();
    let hint = flag(flags, "extract-hint");
    let min_confidence = match flag(flags, "min-confidence") {
        Some(s) => s
            .parse::<f64>()
            .map_err(|e| format!("bad --min-confidence: {e}"))?,
        None => 0.0,
    };
    // The store drops an unrecognized role (forward-compatible, which MCP
    // clients rely on), but a typo'd flag silently losing data is a bad trade
    // for a human at a terminal — reject it before anything runs.
    let role = flags.get("role").map(String::as_str);
    if let Some(r) = role.filter(|r| areev_core::types::Role::from_str(r).is_none()) {
        return Err(format!("--role {r:?}: expected user|assistant|system|tool"));
    }

    // Host-supplied drafts short-circuit the model entirely.
    let explicit = match flag(flags, "facts") {
        Some(j) => Some(areev_store::FactDraft::from_json_array(&j).map_err(|e| e.to_string())?),
        None => None,
    };
    let llm = match explicit {
        Some(_) => None,
        None => resolve_llm(flags, "llm-cmd", "model")?,
    };
    let grounder = match llm {
        Some(_) => resolve_llm(flags, "ground-cmd", "ground-model")?,
        None => None,
    };
    // Under an egress policy, extraction requests leave the process
    // pseudonymized and responses come back rehydrated (D5/D6). Fail-closed:
    // the wrap itself erroring fails remember, it never falls back to raw.
    let wrap = |b: Box<dyn areev_loop::LlmBackend>| -> Result<Box<dyn areev_loop::LlmBackend>, String> {
        if egress_wrap {
            let policy = areev_core::anon::AnonPolicy {
                scope: "session".into(),
                ..Default::default()
            };
            Ok(Box::new(
                areev_llm::PseudonymizingBackend::new(b, policy).map_err(|e| e.to_string())?,
            ))
        } else {
            Ok(b)
        }
    };
    let llm = llm.map(wrap).transpose()?;
    let grounder = grounder.map(wrap).transpose()?;
    let model = llm.as_ref().map(|l| l.model().to_string());

    // --dry-run: extract and print, write nothing. For iterating on
    // --extract-hint without leaving drafts behind in the memory.
    if dry_run {
        let (proposed, facts) = match &llm {
            Some(l) => {
                let (proposed, facts, _) = extract_drafts(
                    l.as_ref(),
                    grounder.as_deref(),
                    "dry-run",
                    &content,
                    hint.as_deref(),
                    min_confidence,
                )?;
                (proposed, facts)
            }
            None => {
                let f = explicit.unwrap_or_default();
                (f.len(), f)
            }
        };
        println!(
            "{}",
            serde_json::json!({
                "dry_run": true,
                "model": model,
                "proposed": proposed,
                "facts": facts.iter().map(draft_json).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }

    // The raw text lands first and unconditionally: whatever the model does
    // next, the source of the extraction is already durable.
    let capture = areev_store::Capture {
        observer: Some(observer.as_str()),
        session_id: flags.get("session-id").map(String::as_str),
        role,
        run_id: flags.get("run-id").map(String::as_str),
    };
    let event = m
        .capture(ns, &content, &capture)
        .map_err(|e| e.to_string())?;
    let report = |facts: &[Hash], extra: serde_json::Value| {
        let mut out = serde_json::json!({
            "event": event.to_hex(),
            "facts": facts.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        });
        if let (Some(obj), Some(extra)) = (out.as_object_mut(), extra.as_object()) {
            obj.extend(extra.clone());
        }
        println!("{out}");
    };
    // A model call that fails still leaves the raw text stored — print the
    // hash so the caller can retry the extraction against it, then exit
    // non-zero so the missing facts are not mistaken for "nothing to extract".
    let failed = |e: String| -> String {
        report(&[], serde_json::json!({ "error": e }));
        e
    };

    let (proposed, drafts, status) = match &llm {
        None => {
            let d = explicit.unwrap_or_default();
            (d.len(), d, None)
        }
        Some(l) => match extract_drafts(
            l.as_ref(),
            grounder.as_deref(),
            &event.to_hex(),
            &content,
            hint.as_deref(),
            min_confidence,
        ) {
            Ok((proposed, drafts, grounded)) => (
                proposed,
                drafts,
                Some(if grounded { "verified" } else { "unverified" }),
            ),
            Err(e) => return Err(failed(e)),
        },
    };

    let attribution = areev_store::FactAttribution {
        verification_status: status,
        extractor_model: model.as_deref(),
    };
    let facts = m
        .attach_facts(ns, &event, &drafts, &attribution)
        .map_err(|e| e.to_string())?;
    let extra = match &model {
        // What the model proposed vs what survived the confidence floor and
        // the grounder — a silent drop would read as "the model found less".
        Some(model) => serde_json::json!({
            "model": model,
            "verification_status": status,
            "proposed": proposed,
            "dropped": proposed.saturating_sub(facts.len()),
        }),
        None => serde_json::json!({}),
    };
    report(&facts, extra);
    Ok(())
}

/// Run the shared extract → confidence floor → ground pipeline and convert the
/// survivors to store drafts. Warns on stderr when a response hit the per-call
/// cap, so a truncated extraction is never silent.
fn extract_drafts(
    llm: &dyn areev_loop::LlmBackend,
    grounder: Option<&dyn areev_loop::LlmBackend>,
    source: &str,
    content: &str,
    hint: Option<&str>,
    min_confidence: f64,
) -> Result<(usize, Vec<areev_store::FactDraft>, bool), String> {
    let ex =
        areev_llm::extract_pipeline(llm, grounder, source, content, hint, min_confidence)
            .map_err(|e| e.to_string())?;
    if ex.proposed >= areev_llm::extract::MAX_FACTS {
        eprintln!(
            "note: extraction hit the {}-fact cap; some facts may have been dropped",
            areev_llm::extract::MAX_FACTS
        );
    }
    let drafts = ex
        .facts
        .into_iter()
        .map(|f| areev_store::FactDraft {
            subject: f.subject,
            relation: f.relation,
            object: f.object,
            confidence: f.confidence,
        })
        .collect();
    Ok((ex.proposed, drafts, ex.grounded))
}

fn draft_json(d: &areev_store::FactDraft) -> serde_json::Value {
    serde_json::json!({
        "subject": d.subject,
        "relation": d.relation,
        "object": d.object,
        "confidence": d.confidence,
    })
}

/// `areev auth mint|list|revoke` — the credential-map lifecycle (A1).
///
/// The map is host config: it holds no policy, no raw secrets, and names no
/// memory, so none of these verbs open a store. They exist because the
/// alternative — hand-editing JSON and hand-computing SHA-256 — is how
/// operator-chosen, low-entropy tokens end up with their digests in a file
/// that gets shared across server instances.
fn run_auth(flags: &HashMap<String, String>, positional: &[String]) -> Result<(), String> {
    use areev_core::authz::{CredentialMap, TOKEN_PREFIX};

    let sub = positional.first().map(String::as_str).unwrap_or("");
    let path = flag(flags, "auth")
        .ok_or_else(|| "areev auth: --auth <FILE> names the credential map".to_string())?;

    // Parse `--expires 90d|12h|2026-12-31T23:59:59Z` to an ISO-8601 instant.
    let expires_at = match flag(flags, "expires") {
        None => None,
        Some(spec) => Some(parse_expiry(&spec)?),
    };

    match sub {
        "mint" => {
            let principal = flag(flags, "principal").ok_or_else(|| {
                "areev auth mint: --principal <NAME> says who the token authenticates as".to_string()
            })?;
            let id = flag(flags, "id").ok_or_else(|| {
                "areev auth mint: --id <NAME> names this credential so it can be revoked \
                 independently of the principal's other tokens"
                    .to_string()
            })?;

            // 256 bits from the OS CSPRNG. This is the whole point of the
            // verb: the operator never chooses the secret, so its entropy is
            // never a question and the stored SHA-256 is never grindable.
            let mut raw = [0u8; 32];
            getrandom::getrandom(&mut raw)
                .map_err(|e| format!("areev auth mint: no system randomness available: {e}"))?;
            let token = format!("{TOKEN_PREFIX}{}", areev_core::authz::encode_token_body(&raw));
            let digest = {
                use sha2::Digest;
                hex::encode(sha2::Sha256::digest(token.as_bytes()))
            };

            // Read-modify-write the map, refusing a duplicate id BEFORE the
            // token is printed — otherwise the operator has a live secret
            // that was never recorded anywhere.
            let mut map = read_map_or_empty(&path)?;
            if map.tokens.iter().any(|t| t.id() == id) {
                return Err(format!(
                    "areev auth mint: {path} already has a credential with id {id:?} — \
                     revoke it first, or choose another id"
                ));
            }
            map.tokens.push(areev_core::authz::CredentialEntry {
                id: Some(id.clone()),
                label: flag(flags, "label"),
                sha256: Some(digest),
                env: None,
                principal: principal.clone(),
                memories: flag(flags, "memories")
                    .map(|m| m.split(',').map(|s| s.trim().to_string()).collect()),
                expires_at: expires_at.clone(),
            });
            // Validate through the same loader the server uses, so a map this
            // verb wrote can never be one the server refuses to load.
            let json = serde_json::to_string_pretty(&map)
                .map_err(|e| format!("areev auth mint: {e}"))?;
            CredentialMap::from_json(&json).map_err(|e| format!("areev auth mint: {e}"))?;
            write_map(&path, &json)?;

            // The token goes to STDOUT (pipeable into a secret store); the
            // commentary to STDERR, so `... | pbcopy` copies only the secret.
            println!("{token}");
            eprintln!("areev: minted {id} for {principal} — recorded in {path}");
            if let Some(exp) = &expires_at {
                eprintln!("areev: expires {exp}");
            }
            eprintln!(
                "areev: this is the ONLY time the token is shown; {path} stores its SHA-256, \
                 which cannot be reversed. Store it now."
            );
        }
        "list" => {
            let map = read_map_or_empty(&path)?;
            let now = areev_core::time::now_ms();
            if map.tokens.is_empty() {
                eprintln!("areev: {path} holds no credentials");
                return Ok(());
            }
            for t in &map.tokens {
                // Never the digest and never the env var's VALUE — listing
                // must be safe to paste into a ticket.
                let state = match t.expires_at.as_deref() {
                    None => "no expiry".to_string(),
                    Some(raw) if t.is_expired_at(now) => format!("EXPIRED {raw}"),
                    Some(raw) => format!("expires {raw}"),
                };
                let scope = match &t.memories {
                    Some(m) => format!(" memories={}", m.join(",")),
                    None => String::new(),
                };
                let label = t.label.as_deref().map(|l| format!(" — {l}")).unwrap_or_default();
                println!("{}\t{}\t{state}{scope}{label}", t.id(), t.principal);
            }
        }
        "revoke" => {
            let id = flag(flags, "id")
                .ok_or_else(|| "areev auth revoke: --id <NAME> (see `areev auth list`)".to_string())?;
            let mut map = read_map_or_empty(&path)?;
            let before = map.tokens.len();
            map.tokens.retain(|t| t.id() != id);
            if map.tokens.len() == before {
                return Err(format!("areev auth revoke: {path} has no credential with id {id:?}"));
            }
            let json = serde_json::to_string_pretty(&map)
                .map_err(|e| format!("areev auth revoke: {e}"))?;
            write_map(&path, &json)?;
            eprintln!(
                "areev: revoked {id} from {path} — RESTART `areev ui` for it to take effect \
                 (the map is read once at startup)"
            );
        }
        other => {
            return Err(format!(
                "unknown auth subcommand {other:?} — one of: mint, list, revoke\n\n\
                 areev auth mint   --auth FILE --id NAME --principal NAME [--label TEXT]\n\
                 \x20                     [--expires 90d|2026-12-31T23:59:59Z] [--memories A,B]\n\
                 areev auth list   --auth FILE\n\
                 areev auth revoke --auth FILE --id NAME"
            ));
        }
    }
    Ok(())
}

/// `90d` / `12h` / an explicit ISO-8601 instant → an ISO-8601 instant.
fn parse_expiry(spec: &str) -> Result<String, String> {
    let spec = spec.trim();
    if let Some(ms) = areev_core::time::iso8601_to_ms(spec) {
        // Already a timestamp — normalize through the same formatter so the
        // file only ever holds one spelling.
        return Ok(format_epoch_ms(ms));
    }
    let (num, unit) = spec.split_at(spec.len().saturating_sub(1));
    let n: i64 = num
        .parse()
        .map_err(|_| format!("--expires {spec:?}: expected <n>d, <n>h, or an ISO-8601 instant"))?;
    let ms = match unit {
        "d" => n * 86_400_000,
        "h" => n * 3_600_000,
        _ => {
            return Err(format!(
                "--expires {spec:?}: unit must be 'd' (days) or 'h' (hours), or give a full \
                 ISO-8601 instant"
            ))
        }
    };
    if n <= 0 {
        return Err(format!("--expires {spec:?}: must be in the future"));
    }
    Ok(format_epoch_ms(areev_core::time::now_ms() + ms))
}

/// Epoch ms → `YYYY-MM-DDTHH:MM:SSZ`, the one spelling the map stores.
fn format_epoch_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Inverse of `days_from_civil` (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Read the credential map, or an empty one when the file does not exist yet
/// (so `areev auth mint` bootstraps a new deployment in one command).
fn read_map_or_empty(path: &str) -> Result<areev_core::authz::CredentialMap, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => areev_core::authz::CredentialMap::from_json(&s).map_err(|e| format!("{path}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(areev_core::authz::CredentialMap {
                version: 1,
                tokens: Vec::new(),
                groups: None,
            })
        }
        Err(e) => Err(format!("{path}: {e}")),
    }
}

/// Write the map with owner-only permissions.
///
/// The file holds no reversible secret, but it does enumerate every principal
/// and credential id a deployment has — a map that is world-readable is a map
/// an attacker uses to pick a target before guessing.
fn write_map(path: &str, json: &str) -> Result<(), String> {
    std::fs::write(path, format!("{json}\n")).map_err(|e| format!("{path}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Load the host policy from `--policy FILE` or `$AREEV_LOOP_POLICY` (§6.2).
fn load_policy(flags: &HashMap<String, String>) -> Result<Option<Policy>, String> {
    let path = flag(flags, "policy").or_else(|| std::env::var("AREEV_LOOP_POLICY").ok());
    match path {
        Some(p) => {
            let s = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
            Ok(Some(Policy::from_json(&s).map_err(|e| e.to_string())?))
        }
        None => Ok(None),
    }
}

/// `areev audit export [--since MS] [--until MS] [--out FILE]` — the
/// accountability evidence bundle (GDPR Art. 5(2)/30, and the Art. 33
/// breach-notification input).
///
/// Two hash-chained trails already exist as grains; this formats them,
/// nothing more. Tier-2 destruction Observations live in `agent:authz`
/// (one per FORGET / FORGET SUBJECT / PURGE, from CAL or the CLI alike),
/// and the loop's lifecycle audit rides as `loop_audit` Facts in
/// `areev-loop`. Output is JSONL — one record per line, oldest first — so
/// extracting a window is a pipe, not a parser.
///
/// **The chain is verified, not assumed.** Each loop audit record carries
/// `derived_from = [rec_hash, previous_audit_hash]`; a record whose
/// predecessor is absent from the export means the trail was truncated,
/// and evidence that cannot say so is worse than none. Breaks are reported
/// on stderr and marked on the record.
fn run_audit(
    m: &mut Areev,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("export");
    if sub_cmd != "export" {
        return Err(format!(
            "unknown audit subcommand '{sub_cmd}' — usage: areev audit export \
             [--since MS] [--until MS] [--out FILE]"
        ));
    }
    let parse_ms = |k: &str| -> Result<Option<i64>, String> {
        match flag(flags, k) {
            None => Ok(None),
            Some(v) => v
                .parse::<i64>()
                .map(Some)
                .map_err(|_| format!("--{k} takes epoch milliseconds, got '{v}'")),
        }
    };
    let since = parse_ms("since")?;
    let until = parse_ms("until")?;
    let in_window = |at: i64| since.is_none_or(|s| at >= s) && until.is_none_or(|u| at <= u);

    // Tier-2 destruction audit: Observations in the reserved authz namespace.
    let mut rows: Vec<(i64, serde_json::Value)> = Vec::new();
    let authz = m
        .recent(
            areev_core::authz::AUTHZ_NS,
            Some(areev_core::types::GrainType::Observation),
            usize::MAX / 2,
        )
        .map_err(|e| e.to_string())?;
    for g in authz {
        let ctx = g.fields.get("context").cloned().unwrap_or(serde_json::Value::Null);
        if ctx.get("audit").and_then(|v| v.as_str()) != Some("tier2") {
            continue;
        }
        let at = g.fields.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0);
        if !in_window(at) {
            continue;
        }
        rows.push((
            at,
            serde_json::json!({
                "trail": "destruction",
                "hash": g.hash.to_hex(),
                "at_ms": at,
                "principal": g.fields.get("observer_id"),
                "observer_type": g.fields.get("observer_type"),
                "verb": ctx.get("verb"),
                // For subject erasures this is `subject:<fingerprint> ns:<ns>`:
                // verify it against a candidate identity by recomputing
                // sha256(id)[..8] in hex; it deliberately cannot be mined
                // for who was erased. `subject_ref` names the scheme.
                "target": ctx.get("target"),
                "subject_ref": ctx.get("subject_ref"),
                "because": ctx.get("because"),
                "grains_erased": ctx.get("grains_erased"),
                "stale_corpora": ctx.get("stale_corpora"),
                "stale_adapters": ctx.get("stale_adapters"),
            }),
        ));
    }

    // Loop lifecycle audit: `loop_audit` Facts carrying the engine's
    // field-map in `object`, hash-chained through `derived_from`.
    let mut chain_prev: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut loop_rows: Vec<(i64, serde_json::Value, Option<String>)> = Vec::new();
    let loop_grains = m
        .recent("areev-loop", Some(areev_core::types::GrainType::Fact), usize::MAX / 2)
        .map_err(|e| e.to_string())?;
    for g in loop_grains {
        if g.get_str("relation") != Some("loop_audit") {
            continue;
        }
        let Some(body) = g.get_str("object").and_then(|o| {
            serde_json::from_str::<serde_json::Value>(o).ok()
        }) else {
            continue;
        };
        let at = body.get("at_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        if !in_window(at) {
            continue;
        }
        chain_prev.insert(g.hash.to_hex());
        // derived_from = [rec_hash, previous_audit_hash?]
        let prev = body
            .get("derived_from")
            .and_then(|v| v.as_array())
            .and_then(|a| a.get(1))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        loop_rows.push((
            at,
            serde_json::json!({
                "trail": "loop",
                "hash": g.hash.to_hex(),
                "at_ms": at,
                "rec_hash": body.get("rec_hash"),
                "from_status": body.get("from_status"),
                "to_status": body.get("to_status"),
                "actor": body.get("actor"),
                "observer_type": body.get("observer_type"),
                "because": body.get("because"),
                "previous_audit": prev.clone(),
            }),
            prev,
        ));
    }
    // Chain verification: a named predecessor absent from this export means
    // the trail is truncated. Report it rather than emitting a clean-looking
    // file.
    let mut breaks = 0usize;
    for (at, mut row, prev) in loop_rows {
        if let Some(p) = prev {
            if !chain_prev.contains(&p) {
                breaks += 1;
                row["chain_break"] = serde_json::json!(true);
            }
        }
        rows.push((at, row));
    }

    rows.sort_by_key(|(at, _)| *at);
    let mut out = String::new();
    for (_, row) in &rows {
        out.push_str(&row.to_string());
        out.push('\n');
    }
    match flag(flags, "out") {
        Some(path) => {
            std::fs::write(&path, &out).map_err(|e| e.to_string())?;
            println!("wrote {} audit records to {path}", rows.len());
        }
        None => print!("{out}"),
    }
    if breaks > 0 {
        eprintln!(
            "areev: warning: {breaks} loop audit record(s) name a predecessor absent from this \
             export — the chain is truncated (widen --since, or the trail was tampered with)"
        );
    }
    eprintln!("audit export: {} records", rows.len());
    Ok(())
}

/// `areev loop <run|list|show|approve|reject|apply|rollback|analyzers|policy|status>`.
fn run_loop(
    m: Areev,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("status");
    let mut sub = AreevSubstrate::new(m, Some(ns.to_string()));
    // Host policy (--policy FILE or $AREEV_LOOP_POLICY) — the only place
    // auto-apply is granted. Absent → a closed default (nothing auto-applies).
    let policy = load_policy(flags)?;
    let mut engine = match policy {
        Some(p) => Engine::with_builtins().with_policy(p),
        None => Engine::with_builtins(),
    };
    // Optional LLM reflection: the model proposes findings that are grounded +
    // adversarially verified before they can reach the queue (origin=llm, never
    // auto-applied). Two ways to attach one, both CLI-only, never persisted:
    //   --model provider:name   → a built-in HTTP backend (key from the env)
    //   --llm-cmd 'CMD'         → a subprocess backend (the zero-dep escape hatch)
    // `--llm-cmd` wins if both are given.
    if let Some(cmd) = flag(flags, "llm-cmd") {
        let model = flag(flags, "llm-model");
        let llm = areev_loop::CommandLlm::new(&cmd, model.as_deref()).map_err(|e| e.to_string())?;
        engine = engine.with_llm(Box::new(llm));
    } else if let Some(spec) = flag(flags, "model") {
        // Key is read from the environment (ANTHROPIC_API_KEY / OPENAI_API_KEY /
        // OLLAMA_HOST, or --llm-api-key-env), never taken on the command line.
        let base = flag(flags, "llm-base-url");
        let key_env = flag(flags, "llm-api-key-env");
        let llm = areev_llm::resolve(&spec, base.as_deref(), key_env.as_deref())
            .map_err(|e| e.to_string())?;
        engine = engine.with_llm(llm);
    }
    // Optional SEPARATE grounding backend (§11): point the entailment check at a
    // cheaper or specialized model, or take the generative model out of grounding
    // entirely. Falls back to the main backend when absent. `--ground-cmd` wins.
    if let Some(cmd) = flag(flags, "ground-cmd") {
        let g = areev_loop::CommandLlm::new(&cmd, None).map_err(|e| e.to_string())?;
        engine = engine.with_ground_llm(Box::new(g));
    } else if let Some(spec) = flag(flags, "ground-model") {
        let base = flag(flags, "llm-base-url");
        let key_env = flag(flags, "llm-api-key-env");
        let g = areev_llm::resolve(&spec, base.as_deref(), key_env.as_deref())
            .map_err(|e| e.to_string())?;
        engine = engine.with_ground_llm(g);
    }
    // Optional external analyzer: a subprocess that flags domain-specific issues
    // (trust class Command → advisory only, never auto-applies). Registered up
    // front so it participates in the pass like a built-in.
    if let Some(cmd) = flag(flags, "analyzer-cmd") {
        let a = areev_loop::CommandAnalyzer::new(&cmd).map_err(|e| e.to_string())?;
        engine.register(Box::new(a));
    }
    let now = now_ms();
    let actor = flag(flags, "actor").unwrap_or_else(|| "user:local".to_string());
    let observer = ObserverType::Human;
    let scopes = ScopeSet::all(); // the CLI is the local root of trust
    let json = flag(flags, "format").as_deref() == Some("json");

    match sub_cmd {
        // `reflect` = a run that re-analyzes the whole memory (full sweep),
        // ignoring the incremental watermark; otherwise identical to `run`.
        "run" | "reflect" => {
            let opts = RunOptions {
                min_new: flag(flags, "min-new").and_then(|v| v.parse().ok()),
                min_new_errors: flag(flags, "min-new-errors").and_then(|v| v.parse().ok()),
                if_stale_ms: flag(flags, "if-stale").and_then(|v| parse_duration(&v)),
                namespaces: Vec::new(),
                full_sweep: sub_cmd == "reflect",
                triggering_actor: Some(actor.clone()),
            };
            let res = engine.run(&mut sub, &opts, now).map_err(|e| e.to_string())?;
            if json {
                println!("{}", serde_json::to_string(&res).map_err(|e| e.to_string())?);
            } else if res.ran() {
                if !flags.contains_key("quiet") {
                    eprintln!(
                        "loop: ran — proposed {} ({} deduped, {} auto-applied) across {} analyzer(s)",
                        res.stored,
                        res.deduped,
                        res.auto_applied,
                        res.analyzers_run.len()
                    );
                }
                if res.stored > 0 {
                    eprintln!("loop: {} new — areev loop list", res.stored);
                }
            } else if !flags.contains_key("quiet") {
                eprintln!("loop: skipped ({:?})", res.skip_reason);
            }
        }
        "list" => {
            let filter = status_filter(flags);
            let recs = engine.recommendations(&sub, filter).map_err(|e| e.to_string())?;
            if json {
                let rows: Vec<_> = recs
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "hash": r.hash,
                            "status": r.status.as_str(),
                            "severity": r.severity.as_str(),
                            "analyzer": r.analyzer,
                            "destructive": r.destructive,
                            "summary": r.summary.render(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string(&rows).map_err(|e| e.to_string())?);
            } else if recs.is_empty() {
                eprintln!("no recommendations — run `areev loop run` first");
            } else {
                for r in &recs {
                    println!(
                        "{}  {:<6}  {:<28}  {}",
                        short(&r.hash),
                        r.severity.as_str(),
                        r.analyzer,
                        r.summary.render()
                    );
                }
            }
            // CI gate: exit 2 if any pending recommendation meets --fail-on.
            if let Some(sev) = flag(flags, "fail-on") {
                let threshold = parse_severity(&sev);
                let hit = recs
                    .iter()
                    .any(|r| r.status == RecStatus::Pending && r.severity >= threshold);
                if hit {
                    eprintln!("loop: pending recommendation(s) at or above severity '{sev}'");
                    std::process::exit(2);
                }
            }
        }
        "show" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| "usage: areev loop show <hash>".to_string())?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            let recs = engine.recommendations(&sub, None).map_err(|e| e.to_string())?;
            let r = recs.iter().find(|r| r.hash == hash).unwrap();
            let mut out = serde_json::json!({
                "hash": r.hash,
                "status": r.status.as_str(),
                "severity": r.severity.as_str(),
                "analyzer": r.analyzer,
                "origin": r.origin,
                "target_ref": r.target_ref,
                "summary": r.summary.render(),
                "destructive": r.destructive,
                "rollbackable": r.rollbackable,
                "evidence": r.evidence,
                "dedup_key": r.dedup_key,
                "confidence": r.confidence,
            });
            // The reviewable change itself — `show` is the review surface, so
            // the proposal must be visible before approve/apply. Flattened
            // (`proposal` kind tag + its fields, e.g. `cal`) exactly like the
            // stored grain body; the outcome metric and any LLM guidance ride
            // along when present.
            let o = out.as_object_mut().unwrap();
            if let Ok(serde_json::Value::Object(p)) = serde_json::to_value(&r.proposal) {
                o.extend(p);
            }
            if let Some(m) = &r.metric {
                o.insert("metric".into(), serde_json::to_value(m).map_err(|e| e.to_string())?);
            }
            if let Some(gd) = &r.guidance {
                o.insert("guidance".into(), serde_json::Value::from(gd.clone()));
            }
            println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
        }
        "approve" | "reject" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| format!("usage: areev loop {sub_cmd} <hash> --because \"...\""))?;
            let because = need(flags, "because")?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            let decision = if sub_cmd == "approve" { Decision::Approve } else { Decision::Reject };
            engine
                .review(&mut sub, &hash, decision, &actor, observer, &scopes, &because, now)
                .map_err(|e| e.to_string())?;
            eprintln!("{sub_cmd}d {}", short(&hash));
        }
        "apply" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| "usage: areev loop apply <hash> --because \"...\" [--gating-run RUN_ID]".to_string())?;
            let because = need(flags, "because")?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            let allow_destructive = flags.contains_key("allow-destructive");
            // §7.4: a code revision applies only through the recorded
            // evalset-run edge. The evidence is LOADED from the summary the
            // gate run wrote — never typed in by the operator.
            let applied = match flag(flags, "gating-run") {
                Some(run_id) => {
                    let gating = load_gating_evidence(&sub, &engine, &hash, &run_id)?;
                    engine
                        .apply_gated(&mut sub, &hash, &actor, observer, &scopes, &because, allow_destructive, &gating, now)
                        .map_err(|e| e.to_string())?
                }
                None => engine
                    .apply(&mut sub, &hash, &actor, observer, &scopes, &because, allow_destructive, now)
                    .map_err(|e| e.to_string())?,
            };
            eprintln!(
                "applied {} ({})",
                short(&hash),
                if applied.rollbackable { "rollbackable" } else { "non-rollbackable" }
            );
        }
        "rollback" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| "usage: areev loop rollback <hash> --because \"...\"".to_string())?;
            let because = need(flags, "because")?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            // §7.4 blast radius: reverting CODE does not revert the data the
            // code produced. Before the revert lands, report which runs
            // touched the reverted tool — the report is what makes that
            // data reviewable, so it prints even (especially) when large.
            let target_ref = engine
                .recommendations(&sub, None)
                .ok()
                .and_then(|recs| recs.into_iter().find(|r| r.hash == hash))
                .map(|r| r.target_ref);
            // The adapter mirror: retracting a promotion is the memory-side
            // inverse — the SERVING side is the host's move, and the runs the
            // adapter answered are not reverted. The opaque is a served-model
            // name, not a grain hash, so there is no runs_touching walk.
            if let Some(model) = target_ref.as_deref().and_then(|t| t.strip_prefix("model:")) {
                eprintln!(
                    "adapter promotion for model:{model} will be retracted — the host \
                     must stop serving it; runs answered since promotion are NOT reverted"
                );
            }
            let code_target = target_ref
                .as_deref()
                .and_then(|t| t.strip_prefix("tool:").map(str::to_string));
            if let Some(tool_hash) = &code_target {
                if let Ok(h) = Hash::from_hex(tool_hash) {
                    let touched = sub
                        .facade()
                        .with_store(|m| m.runs_touching("agent:harness", &h, 2))
                        .unwrap_or_default();
                    eprintln!(
                        "blast radius: {} run(s) touched tool {} since apply{}",
                        touched.len(),
                        short(tool_hash),
                        if touched.is_empty() { String::new() } else { format!(": {}", touched.join(", ")) }
                    );
                    eprintln!(
                        "  the runs' outputs are NOT reverted — inspect each with \
                         `areev run-trace --run-id <id>`"
                    );
                }
            }
            engine
                .rollback(&mut sub, &hash, &actor, observer, &scopes, &because, now)
                .map_err(|e| e.to_string())?;
            eprintln!("rolled back {}", short(&hash));
        }
        "analyzers" => {
            // One row per registered analyzer: id, tier, trust class
            // (builtin vs an external command — advisory only), default state,
            // title. Same lowercase trust strings as the console's Setup view.
            for a in engine.analyzers() {
                let m = a.manifest();
                println!(
                    "{:<28}  {:?}  {:<7}  on={}  {}",
                    m.id,
                    m.tier,
                    format!("{:?}", m.trust_class).to_lowercase(),
                    m.default_on,
                    m.title
                );
            }
        }
        // The Verify gate's measured history: did applied advice hold?
        "outcomes" => {
            let outcomes = engine.outcomes(&sub).map_err(|e| e.to_string())?;
            if json {
                println!("{}", serde_json::to_string(&outcomes).map_err(|e| e.to_string())?);
            } else if outcomes.is_empty() {
                eprintln!(
                    "no measured outcomes yet — outcome review runs after an applied \
                     recommendation's review window elapses"
                );
            } else {
                for o in &outcomes {
                    let horizon = if o.horizon_ms % 86_400_000 == 0 {
                        format!("{}d", o.horizon_ms / 86_400_000)
                    } else {
                        format!("{}h", o.horizon_ms / 3_600_000)
                    };
                    println!(
                        "{}  {:<22}  @{:<4}  baseline {} → current {}  [{}]",
                        short(&o.rec_hash),
                        o.metric,
                        horizon,
                        o.baseline,
                        o.current,
                        o.verdict
                    );
                }
            }
        }
        // Config reporting: the effective host policy (read-only).
        "policy" => {
            println!(
                "{}",
                serde_json::to_string_pretty(engine.policy()).map_err(|e| e.to_string())?
            );
        }
        // Bare `areev loop` (or an unknown subcommand) prints a health summary.
        _ => {
            let h = engine.health(&sub, now).map_err(|e| e.to_string())?;
            if json {
                println!("{}", serde_json::to_string(&h).map_err(|e| e.to_string())?);
            } else {
                println!(
                    "loop: {} recommendation(s) — {} pending, {} applied",
                    h.total, h.pending, h.applied
                );
                match h.last_run_ms {
                    None => println!("  never run — areev loop run"),
                    Some(last) => {
                        let days = (now - last) / 86_400_000;
                        println!(
                            "  last run {days}d ago; {} new grain(s) ({} tool error(s)) since",
                            h.grains_since_run, h.error_events_since_run
                        );
                    }
                }
                // Reflection §6b: the live approval-rate for LLM-surfaced
                // findings (only shown once the LLM path has produced any).
                let lm = engine.llm_metrics(&sub).map_err(|e| e.to_string())?;
                if lm.proposed > 0 {
                    match lm.approval_rate {
                        Some(rate) => println!(
                            "  LLM findings: {} surfaced, {:.0}% approved ({} approved / {} rejected, {} pending)",
                            lm.proposed, rate * 100.0, lm.approved, lm.rejected, lm.pending
                        ),
                        None => println!("  LLM findings: {} surfaced, none decided yet", lm.proposed),
                    }
                }
                if h.stale {
                    eprintln!("  ⚠ the loop may be stale — run `areev loop run` or wire the SessionEnd hook");
                } else if h.pending > 0 {
                    println!("  review with: areev loop list");
                }
            }
        }
    }
    Ok(())
}

/// `areev init --db <file> [--template blank|demo|coding-agent] [--ns N]` —
/// seed a working backend and print the wiring snippet (never writes settings).
fn run_init(
    m: Areev,
    ns: &str,
    flags: &HashMap<String, String>,
    _positional: &[String],
) -> Result<(), String> {
    let template = flag(flags, "template").unwrap_or_else(|| "blank".to_string());
    let mut m = m;
    let seeded = match template.as_str() {
        "blank" => 0,
        "demo" => seed_demo(&mut m, ns)?,
        "coding-agent" | "support-agent" => {
            m.add(&Fact::new("agent", "instruction", "review pending recommendations before acting").namespace(ns))
                .map_err(|e| e.to_string())?;
            1
        }
        other => return Err(format!("unknown --template '{other}' (blank|demo|coding-agent|support-agent)")),
    };

    println!("initialized backend (template: {template}, seeded {seeded} grain(s))");
    if template == "demo" {
        println!("next: areev loop run --db <file>   # ~4 recommendations across analyzers");
    }
    // Print the Claude Code hook snippet — areev never edits your settings.
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "areev".to_string());
    eprintln!("\nClaude Code hooks (paste into settings.json — absolute path baked in):");
    eprintln!("  UserPromptSubmit → {exe} recall-hook --ns {ns} --with-loop");
    eprintln!("  Stop             → {exe} capture-stop --ns {ns}");
    eprintln!("  SessionEnd       → {exe} loop run --min-new 20 --min-new-errors 3 --quiet --ns {ns}");
    Ok(())
}

/// Seed the demo corpus: planted duplicates, a contradiction, and a stale
/// grain, so the first `areev loop run` fires several analyzers at once.
fn seed_demo(m: &mut Areev, ns: &str) -> Result<usize, String> {
    let past = 1_000_000_000_000; // year 2001 — safely elapsed
    let grains = [
        Fact::new("acme", "tier", "Enterprise").namespace(ns),
        Fact::new("acme", "tier", "Enterprise").namespace(ns),
        Fact::new("acme", "deploy_target", "us-east-1").namespace(ns),
        Fact::new("acme", "deploy_target", "eu-west-1").namespace(ns),
        Fact::new("promo", "active", "true").namespace(ns).valid_to(past),
    ];
    for g in &grains {
        m.add(g).map_err(|e| e.to_string())?;
    }
    Ok(grains.len())
}

/// The stack `run` is given, independent of the platform default.
///
/// Windows hands a process's main thread 1 MiB; Linux and macOS hand it 8.
/// The deepest CLI paths — `loop apply`, which threads the giant `run`
/// dispatcher frame through the engine, the substrate adapter, the CAL facade
/// and the store — sit just over that 1 MiB in a debug build, so the same
/// command that works everywhere else aborted on Windows with
/// STATUS_STACK_OVERFLOW and no message. Depending on the host default meant
/// depending on a number the platform picks; this picks it.
const CLI_STACK_BYTES: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    // Run the whole CLI on a thread whose stack size we chose. Cheap (one
    // spawn per process) and it makes stack headroom identical on every
    // platform instead of 8x smaller on one of them.
    let worker = std::thread::Builder::new()
        .stack_size(CLI_STACK_BYTES)
        .spawn(run);
    let result = match worker {
        Ok(h) => h.join(),
        // Spawning failed (a resource limit, not a bug in `run`): fall back to
        // this thread rather than refusing to run at all.
        Err(_) => Ok(run()),
    };
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) => {
            eprintln!("areev: {e}");
            ExitCode::FAILURE
        }
        // The worker panicked. Its own hook has already printed the panic;
        // adding a second message would just bury it.
        Err(_) => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> (HashMap<String, String>, Vec<String>) {
        parse_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn valueless_long_flag_does_not_swallow_short_flag() {
        // Regression: in `serve --mcp -d mem.db`, the valueless `--mcp` must not
        // consume `-d` as its value — otherwise --db is never set and serve
        // silently falls back to the default memory file.
        let (flags, pos) = args(&["--mcp", "-d", "mem.db", "--ns", "caller"]);
        assert_eq!(flags.get("mcp").map(String::as_str), Some("true"));
        assert_eq!(flags.get("db").map(String::as_str), Some("mem.db"));
        assert_eq!(flags.get("ns").map(String::as_str), Some("caller"));
        assert!(pos.is_empty(), "no stray positionals: {pos:?}");
    }

    #[test]
    fn db_flag_equivalent_across_form_and_order() {
        for a in [
            &["--mcp", "-d", "mem.db"][..],
            &["-d", "mem.db", "--mcp"][..],
            &["--mcp", "--db", "mem.db"][..],
            &["--db", "mem.db", "--mcp"][..],
        ] {
            let (flags, _) = args(a);
            assert_eq!(flags.get("db").map(String::as_str), Some("mem.db"), "args: {a:?}");
            assert_eq!(flags.get("mcp").map(String::as_str), Some("true"), "args: {a:?}");
        }
    }

    #[test]
    fn adjacent_valueless_flags_stay_true() {
        let (flags, _) = args(&["--mcp", "--no-destructive-ops"]);
        assert_eq!(flags.get("mcp").map(String::as_str), Some("true"));
        assert_eq!(flags.get("no-destructive-ops").map(String::as_str), Some("true"));
    }

    #[test]
    fn openai_tool_import_preserves_arguments_error_and_call_id() {
        let rows = extract_tool_records(&serde_json::json!({
            "created_at": 42,
            "tool_calls": [{
                "id": "call-7",
                "is_error": true,
                "function": {
                    "name": "charge",
                    "arguments": "{\"amount\":42}"
                }
            }]
        }));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "charge");
        assert_eq!(rows[0].input, Some(serde_json::json!({"amount": 42})));
        assert!(rows[0].content.is_empty(), "arguments are not a result");
        assert!(rows[0].is_error);
        assert_eq!(rows[0].call_id.as_deref(), Some("call-7"));
        assert_eq!(rows[0].created_at, Some(42));
    }
}

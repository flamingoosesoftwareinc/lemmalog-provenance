//! The LLM integration layer (Phase 4 of the design).
//!
//! The fixpoint never contains an LLM call: extraction happens at the
//! *ingestion boundary* (the [`Extractor`] trait — a real deployment plugs an
//! OpenIE LLM here, tests and examples use [`MockExtractor`]), update
//! decisions are deterministic rules first (Mem0-style
//! ADD/UPDATE/NOOP/escalate), and derivation runs asynchronously via
//! `maintain()`. The [`ContextAssembler`] places distilled facts at the top
//! of the window and verbatim provenance at the bottom (lost-in-the-middle
//! mitigation) under a token budget.

use crate::eval::{Ann, Engine};
use crate::intern::Value;
use crate::intern::Term;
use std::collections::HashMap;
use std::fmt::Write as _;

/// A conversational episode: the unit of ingestion and provenance.
#[derive(Debug, Clone)]
pub struct Episode {
    pub id: String,
    pub text: String,
    pub ts: i64,
    /// The identified speaker, when the application knows it: first-person
    /// references in the episode resolve to this entity during extraction.
    pub speaker: Option<String>,
}

/// One candidate fact produced by extraction.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateFact {
    pub subj: String,
    pub pred: String,
    pub obj: String,
    pub confidence: f64,
}

/// The extraction boundary. Implementations call an LLM in production;
/// they must be memoizable by (episode, extractor-version).
pub trait Extractor {
    fn extract(&mut self, episode: &Episode) -> Vec<CandidateFact>;

    /// Observability: (model calls, failures). Defaults to zero for
    /// deterministic extractors.
    fn stats(&self) -> (usize, usize) {
        (0, 0)
    }
}

/// Boxed extractors are extractors: lets callers swap implementations
/// (live vs file-cached) behind one `AgentMemory` type.
impl Extractor for Box<dyn Extractor> {
    fn extract(&mut self, episode: &Episode) -> Vec<CandidateFact> {
        (**self).extract(episode)
    }

    fn stats(&self) -> (usize, usize) {
        (**self).stats()
    }
}

/// Deterministic stand-in for the LLM OpenIE step: parses `S --rel--> O`
/// lines at fixed confidence. Used by tests and examples.
pub struct MockExtractor {
    pub confidence: f64,
    seen: HashMap<String, Vec<CandidateFact>>,
}

impl MockExtractor {
    pub fn new(confidence: f64) -> Self {
        MockExtractor {
            confidence,
            seen: HashMap::new(),
        }
    }
}

impl Extractor for MockExtractor {
    fn extract(&mut self, episode: &Episode) -> Vec<CandidateFact> {
        // memoized by episode id: never re-extracted
        if let Some(cached) = self.seen.get(&episode.id) {
            return cached.clone();
        }
        let out = parse_protocol(episode.text.as_str(), self.confidence);
        self.seen.insert(episode.id.clone(), out.clone());
        out
    }
}

/// Why an entity token fails strict validation, as a self-correcting
/// reason (echoed to the model that produced it).
fn entity_token_problem(s: &str) -> Option<String> {
    // unresolved-reference words: pronouns and role placeholders that mean
    // the model failed to resolve the entity
    const BLOCKED: [&str; 14] = [
        "i", "me", "my", "mine", "speaker", "user", "they", "them", "he", "she",
        "it", "we", "you", "that",
    ];
    let lower = s.to_lowercase();
    if s.is_empty() {
        Some("empty entity name".to_string())
    } else if BLOCKED.contains(&lower.as_str()) {
        Some(format!(
            "'{s}' is a pronoun or role word — resolve it to the entity's real name"
        ))
    } else if s.len() > 60 || s.split_whitespace().count() > 8 {
        Some("looks like prose (more than 8 words) — entity names are short".to_string())
    } else if !s
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '\'' | ' '))
    {
        Some("contains punctuation or prose characters — entity names use letters, digits, '_', '-', apostrophes".to_string())
    } else {
        None
    }
}

fn valid_entity_token(s: &str) -> bool {
    entity_token_problem(s).is_none()
}

/// Strict protocol parsing for MODEL output: lines that are not exactly
/// `Entity --relation[conf]--> Entity` with clean entity tokens are
/// dropped. Reasoning models sometimes leak deliberation into the answer;
/// those lines (questions, prose, bullets) must not become facts.
pub fn parse_protocol_strict(text: &str, default_confidence: f64) -> Vec<CandidateFact> {
    parse_protocol(text, default_confidence)
        .into_iter()
        .filter(|c| {
            valid_entity_token(&c.subj)
                && valid_entity_token(&c.obj)
                && !c.pred.is_empty()
                && c.pred
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        })
        .collect()
}

/// One line of the protocol, or a reason it cannot parse.
fn parse_line(raw: &str, default_confidence: f64) -> Result<CandidateFact, String> {
    let line = raw.trim();
    let (s, rest) = line
        .split_once("--")
        .ok_or_else(|| "no `--rel-->` structure".to_string())?;
    let (rel, o) = rest
        .split_once("-->")
        .ok_or_else(|| "has `--` but no `-->`".to_string())?;
    // optional confidence suffix on the relation: `rel[0.8]`
    let (rel, conf) = match rel.trim().rsplit_once('[') {
        Some((r, c)) if c.ends_with(']') => (
            r.trim(),
            c.trim_end_matches(']')
                .trim()
                .parse::<f64>()
                .unwrap_or(default_confidence),
        ),
        _ => (rel.trim(), default_confidence),
    };
    if rel.is_empty() {
        return Err("empty relation".to_string());
    }
    Ok(CandidateFact {
        subj: s.trim().to_string(),
        pred: rel.to_string(),
        obj: o.trim().to_string(),
        confidence: conf,
    })
}

/// Line protocol shared by mock and LLM extractors: each line is
/// `S --rel--> O` with optional per-fact confidence `S --rel[0.8]--> O`.
/// Unparseable lines are skipped (extraction is best-effort).
pub fn parse_protocol(text: &str, default_confidence: f64) -> Vec<CandidateFact> {
    text.lines()
        .filter_map(|l| parse_line(l, default_confidence).ok())
        .collect()
}

/// Strict parse WITH a drop report: `(facts, dropped)` where dropped is
/// `(line, reason)` for every line not asserted — parse failures and
/// strict-validation failures alike. Silent zero-fact ingestion is the
/// worst failure mode a caller can face; this makes it loud.
pub fn parse_protocol_reported(
    text: &str,
    default_confidence: f64,
) -> (Vec<CandidateFact>, Vec<(String, String)>) {
    let mut facts = Vec::new();
    let mut dropped = Vec::new();
    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        match parse_line(raw, default_confidence) {
            Ok(c) => {
                let problem = entity_token_problem(&c.subj)
                    .or_else(|| entity_token_problem(&c.obj))
                    .or_else(|| {
                        if c.pred
                            .chars()
                            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
                        {
                            None
                        } else {
                            Some("relation must be lower snake_case (e.g. works_at)".to_string())
                        }
                    });
                match problem {
                    Some(reason) => dropped.push((raw.trim().to_string(), reason)),
                    None => facts.push(c),
                }
            }
            Err(reason) => dropped.push((raw.trim().to_string(), reason)),
        }
    }
    (facts, dropped)
}

/// An [`Extractor`] whose extraction step is a caller-supplied model call —
/// bring your own provider (OpenAI, Anthropic, a local server, a test
/// closure). Lemmalog owns the prompt, the response protocol, and
/// memoization by episode id, so an episode is never re-extracted.
///
/// The model is asked to answer in the line protocol `S --rel--> O`
/// (optionally `S --rel[0.8]--> O`). Extraction failures degrade to zero
/// facts rather than poisoning memory.
pub struct LlmExtractor {
    call: Box<dyn FnMut(&str) -> Result<String, String>>,
    default_confidence: f64,
    seen: HashMap<String, Vec<CandidateFact>>,
    pub calls: usize, // observability for tests/metrics
}

pub const EXTRACTION_PROMPT: &str = "\
Extract the factual triples from the episode below. Answer with one triple \
per line in exactly this format, nothing else:\n\
SUBJECT --RELATION[CONFIDENCE]--> OBJECT\n\
CONFIDENCE is a number in [0,1] (omit [CONFIDENCE] for 0.9). RELATION must \
be one of: works_at, manager, likes, job_title, member_of, located_in, \
links. Employment (works at, joined, was hired by, left) is ALWAYS \
works_at - for a job change emit only the NEW employer as a works_at \
triple. Reporting lines (reports to, manager is) are ALWAYS manager, with \
the person as the subject. Use the closest match for anything else; skip \
facts that fit none. SUBJECT and OBJECT must be real entity names exactly \
as written in the episode: NEVER a pronoun or a role word (speaker, user, \
the manager) - always the full name. Output ONLY the triple lines: no \
reasoning, no explanations, no bullets, no questions. Skip opinions and \
small talk.\n\
Episode:\n";

impl LlmExtractor {
    pub fn new<F>(call: F) -> Self
    where
        F: FnMut(&str) -> Result<String, String> + 'static,
    {
        LlmExtractor {
            call: Box::new(call),
            default_confidence: 0.9,
            seen: HashMap::new(),
            calls: 0,
        }
    }
}

impl Extractor for LlmExtractor {
    fn extract(&mut self, episode: &Episode) -> Vec<CandidateFact> {
        if let Some(cached) = self.seen.get(&episode.id) {
            return cached.clone();
        }
        self.calls += 1;
        let prompt = format!("{EXTRACTION_PROMPT}{}", episode.text);
        let out = match (self.call)(&prompt) {
            Ok(response) => parse_protocol(&response, self.default_confidence),
            Err(_) => Vec::new(), // degraded turn: no facts, no poison
        };
        self.seen.insert(episode.id.clone(), out.clone());
        out
    }

    fn stats(&self) -> (usize, usize) {
        (self.calls, 0)
    }
}

/// Outcome of one `observe()` — the agent-visible update report.
#[derive(Debug, Default, Clone)]
pub struct IngestReport {
    pub added: usize,
    pub updated: usize,
    pub noop: usize,
    pub escalations: Vec<String>,
}

/// Agent memory facade: engine + extraction + episodes + escalations.
pub struct AgentMemory<X: Extractor> {
    pub engine: Engine,
    extractor: X,
    episodes: Vec<Episode>,
    escalations: Vec<String>,
    episode_counter: u64,
    /// Epoch of the last completed `maintain()`; `context()` reports
    /// memory changes since then.
    last_turn_epoch: u64,
    extra_rules: String,
    hyp_counter: u64,
}

pub const DEFAULT_RULES: &str = "\
# temporal projection: what is true NOW
current(E,R,O) :- edge(E,R,O,VF,VT,_), now(T), VF =< T, T < VT.
current(E,R,O) :- scoped_edge(_,E,R,O,VF,VT,_), now(T), VF =< T, T < VT.
scoped_current(S,E,R,O) :- scoped_edge(S,E,R,O,VF,VT,_), now(T), VF =< T, T < VT.
# curated exclusivity table for the update policy
exclusive(\"works_at\").
";

impl<X: Extractor> AgentMemory<X> {
    pub fn new(extractor: X, extra_rules: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let mut engine = Engine::new();
        engine.install_program(DEFAULT_RULES)?;
        if !extra_rules.trim().is_empty() {
            engine.install_program(extra_rules)?;
        }
        Ok(AgentMemory {
            engine,
            extractor,
            episodes: Vec::new(),
            escalations: Vec::new(),
            episode_counter: 0,
            last_turn_epoch: 0,
            extra_rules: extra_rules.to_string(),
            hyp_counter: 0,
        })
    }

    /// Ingest one episode at the current engine time.
    pub fn observe(&mut self, text: &str) -> IngestReport {
        let ts = self.engine.now;
        self.observe_at(text, ts)
    }

    /// Ingest an episode with a known speaker: first-person references
    /// ("I", "my") resolve to `speaker` during extraction.
    pub fn observe_as(&mut self, text: &str, ts: i64, speaker: &str) -> IngestReport {
        self.engine.set_now(ts);
        self.episode_counter += 1;
        let episode = Episode {
            id: format!("ep{}", self.episode_counter),
            text: text.to_string(),
            ts,
            speaker: Some(speaker.to_string()),
        };
        let candidates = self.extractor.extract(&episode);
        let mut report = IngestReport::default();
        for c in &candidates {
            self.apply_update(c, &episode, &mut report, &[]);
        }
        self.episodes.push(episode);
        self.escalations.extend(report.escalations.clone());
        report
    }

    /// Ingest one episode at an explicit timestamp: the extraction boundary
    /// is bi-temporal — `ts` becomes both valid-from of new facts and the
    /// closing valid-to of facts they supersede. Call `maintain()` (the
    /// sleep-time slot) afterwards to re-derive.
    pub fn observe_at(&mut self, text: &str, ts: i64) -> IngestReport {
        self.engine.set_now(ts);
        self.episode_counter += 1;
        let episode = Episode {
            id: format!("ep{}", self.episode_counter),
            text: text.to_string(),
            ts,
            speaker: None,
        };
        let candidates = self.extractor.extract(&episode);
        let mut report = IngestReport::default();
        for c in &candidates {
            self.apply_update(c, &episode, &mut report, &[]);
        }
        self.episodes.push(episode);
        self.escalations.extend(report.escalations.clone());
        report
    }

    /// Deterministic update decision for one candidate fact:
    /// - no open fact with same (S,P)      -> ADD
    /// - open fact with same (S,P,O)       -> NOOP (annotation merge)
    /// - open fact with different O:
    ///     - P exclusive                   -> UPDATE (close old, assert new)
    ///     - otherwise                     -> ADD + escalation
    fn apply_update(
        &mut self,
        c: &CandidateFact,
        ep: &Episode,
        report: &mut IngestReport,
        provenance: &[String],
    ) {
        let subj = self.engine.sym(&c.subj);
        let pred = self.engine.sym(&c.pred);
        let obj = self.engine.sym(&c.obj);
        let mut fact_provenance = vec![ep.id.clone()];
        fact_provenance.extend(provenance.iter().cloned());
        let open: Vec<Vec<Value>> = self
            .engine
            .query("edge", &[Some(subj), Some(pred), None, None, None, None])
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| matches!(k[4].as_int(), Some(vt) if vt == i64::MAX))
            .collect();
        if open.is_empty() {
            self.assert_open(&[subj, pred, obj], c.confidence, &fact_provenance);
            report.added += 1;
            return;
        }
        if open.iter().any(|k| k[2] == obj) {
            // same fact re-observed: merge annotation, no structural change
            let mut k = open[0].clone();
            k[2] = obj;
            self.engine.declare(
                "edge",
                &k,
                Ann::base(c.confidence, fact_provenance.iter().cloned()),
            );
            report.noop += 1;
            return;
        }
        let exclusive = !self.engine.query("exclusive", &[Some(pred)]).is_empty();
        if exclusive {
            for old in &open {
                let mut closed = old.clone();
                closed[4] = Value::Int(self.engine.now);
                self.engine.retract("edge", old);
                self.engine.declare("edge", &closed, Ann::base(0.9, ["superseded"]));
            }
            self.assert_open(&[subj, pred, obj], c.confidence, &fact_provenance);
            report.updated += 1;
        } else {
            self.assert_open(&[subj, pred, obj], c.confidence, &fact_provenance);
            let others: Vec<String> = open
                .iter()
                .map(|k| self.engine.interner.display(&k[2]))
                .collect();
            report.escalations.push(format!(
                "conflict: {} --{}--> {} asserted in {}, but {} also open ({})",
                c.subj, c.pred, c.obj, ep.id, c.pred, others.join(", ")
            ));
            report.added += 1;
        }
    }

    fn assert_open(&mut self, spo: &[Value; 3], conf: f64, provenance: &[String]) {
        let args = vec![
            spo[0],
            spo[1],
            spo[2],
            Value::Int(self.engine.now),
            Value::Int(i64::MAX),
            Value::Int(self.engine.now),
        ];
        self.engine
            .declare("edge", &args, Ann::base(conf, provenance.iter().cloned()));
    }

    /// Advance time and run incremental maintenance (the sleep-time slot).
    /// The epoch the run logs under is remembered so the next `context()`
    /// can report this turn's changes ("what's new in memory").
    pub fn maintain(&mut self, now: i64) -> usize {
        self.engine.set_now(now);
        self.last_turn_epoch = self.engine.epoch();
        self.engine.run()
    }

    pub fn escalations(&self) -> &[String] {
        &self.escalations
    }

    /// Dismiss an escalation (agent resolved it out-of-band).
    pub fn resolve_escalation(&mut self, idx: usize) {
        if idx < self.escalations.len() {
            self.escalations.remove(idx);
        }
    }

    /// Agent-facing read-only query: bindings for an atom like
    /// `current("alice", R, O)` against materialized relations.
    pub fn ask(&self, goal: &str) -> Result<Vec<String>, crate::ast::ParseError> {
        self.engine.ask(goal)
    }

    /// Ingest PRE-PARSED facts (callers that extract themselves, e.g. the
    /// MCP server where the host model does extraction): applies the same
    /// update policy as `observe_at`, and returns the drop report for any
    /// lines the caller's protocol parse rejected.
    pub fn observe_extracted(
        &mut self,
        text: &str,
        ts: i64,
    ) -> (IngestReport, Vec<(String, String)>) {
        self.observe_extracted_with_provenance(text, ts, &[])
    }

    /// Ingest protocol facts with opaque provenance references supplied by a
    /// caller such as a repository-aware CLI. The episode id remains attached
    /// as well, so existing proof trees and snapshots keep their behavior.
    pub fn observe_extracted_with_provenance(
        &mut self,
        text: &str,
        ts: i64,
        provenance: &[String],
    ) -> (IngestReport, Vec<(String, String)>) {
        self.engine.set_now(ts);
        let (candidates, dropped) = parse_protocol_reported(text, 0.9);
        self.episode_counter += 1;
        let episode = Episode {
            id: format!("ep{}", self.episode_counter),
            text: text.to_string(),
            ts,
            speaker: None,
        };
        let mut report = IngestReport::default();
        for c in &candidates {
            self.apply_update(c, &episode, &mut report, provenance);
        }
        self.episodes.push(episode);
        self.escalations.extend(report.escalations.clone());
        (report, dropped)
    }

    /// Ingest protocol facts into an explicit workspace or repository scope.
    /// Scoped facts use a separate base relation so identical triples in two
    /// repositories retain independent update histories.
    pub fn observe_scoped_extracted_with_provenance(
        &mut self,
        text: &str,
        ts: i64,
        scope: &str,
        provenance: &[String],
    ) -> (IngestReport, Vec<(String, String)>) {
        self.engine.set_now(ts);
        let (candidates, dropped) = parse_protocol_reported(text, 0.9);
        self.episode_counter += 1;
        let episode = Episode {
            id: format!("ep{}", self.episode_counter),
            text: text.to_string(),
            ts,
            speaker: None,
        };
        let mut report = IngestReport::default();
        for candidate in &candidates {
            self.apply_scoped_update(candidate, &episode, &mut report, scope, provenance);
        }
        self.episodes.push(episode);
        self.escalations.extend(report.escalations.clone());
        (report, dropped)
    }

    fn apply_scoped_update(
        &mut self,
        candidate: &CandidateFact,
        episode: &Episode,
        report: &mut IngestReport,
        scope: &str,
        provenance: &[String],
    ) {
        let scope_value = self.engine.sym(scope);
        let subj = self.engine.sym(&candidate.subj);
        let pred = self.engine.sym(&candidate.pred);
        let obj = self.engine.sym(&candidate.obj);
        let mut fact_provenance = vec![episode.id.clone()];
        fact_provenance.extend(provenance.iter().cloned());
        let open: Vec<Vec<Value>> = self
            .engine
            .query(
                "scoped_edge",
                &[
                    Some(scope_value),
                    Some(subj),
                    Some(pred),
                    None,
                    None,
                    None,
                    None,
                ],
            )
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| matches!(key[5].as_int(), Some(valid_to) if valid_to == i64::MAX))
            .collect();
        if open.is_empty() {
            self.assert_scoped_open(
                &[scope_value, subj, pred, obj],
                candidate.confidence,
                &fact_provenance,
            );
            report.added += 1;
            return;
        }
        if open.iter().any(|key| key[3] == obj) {
            let mut key = open[0].clone();
            key[3] = obj;
            self.engine.declare(
                "scoped_edge",
                &key,
                Ann::base(candidate.confidence, fact_provenance.iter().cloned()),
            );
            report.noop += 1;
            return;
        }
        let exclusive = !self.engine.query("exclusive", &[Some(pred)]).is_empty();
        if exclusive {
            for old in &open {
                let mut closed = old.clone();
                closed[5] = Value::Int(self.engine.now);
                self.engine.retract("scoped_edge", old);
                self.engine
                    .declare("scoped_edge", &closed, Ann::base(0.9, ["superseded"]));
            }
            self.assert_scoped_open(
                &[scope_value, subj, pred, obj],
                candidate.confidence,
                &fact_provenance,
            );
            report.updated += 1;
        } else {
            self.assert_scoped_open(
                &[scope_value, subj, pred, obj],
                candidate.confidence,
                &fact_provenance,
            );
            let others: Vec<String> = open
                .iter()
                .map(|key| self.engine.interner.display(&key[3]))
                .collect();
            report.escalations.push(format!(
                "conflict in {scope}: {} --{}--> {} asserted in {}, but {} also open ({})",
                candidate.subj,
                candidate.pred,
                candidate.obj,
                episode.id,
                candidate.pred,
                others.join(", ")
            ));
            report.added += 1;
        }
    }

    fn assert_scoped_open(
        &mut self,
        scoped_spo: &[Value; 4],
        confidence: f64,
        provenance: &[String],
    ) {
        let args = vec![
            scoped_spo[0],
            scoped_spo[1],
            scoped_spo[2],
            scoped_spo[3],
            Value::Int(self.engine.now),
            Value::Int(i64::MAX),
            Value::Int(self.engine.now),
        ];
        self.engine.declare(
            "scoped_edge",
            &args,
            Ann::base(confidence, provenance.iter().cloned()),
        );
    }

    /// Restrict this in-memory view to selected scopes. Callers must not save
    /// the filtered view over the complete workspace snapshot.
    pub fn retain_scopes(&mut self, scopes: &[String]) {
        let allowed: std::collections::HashSet<Value> =
            scopes.iter().map(|scope| self.engine.sym(scope)).collect();
        let keys: Vec<Vec<Value>> = self
            .engine
            .query("scoped_edge", &[None, None, None, None, None, None, None])
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| !allowed.contains(&key[0]))
            .collect();
        for key in keys {
            self.engine.retract("scoped_edge", &key);
        }
        self.engine.run();
    }

    /// Remove legacy unscoped facts from an in-memory filtered view.
    pub fn remove_unscoped_facts(&mut self) {
        let keys: Vec<Vec<Value>> = self
            .engine
            .query("edge", &[None, None, None, None, None, None])
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        for key in keys {
            self.engine.retract("edge", &key);
        }
        self.engine.run();
    }

    /// Agent tool surface: install a rule batch (versioned, revertable).
    pub fn install_rules(&mut self, src: &str) -> Result<String, Box<dyn std::error::Error>> {
        self.engine.install_program(src)
    }

    /// Agent tool surface: uninstall a rule batch; derivations revert on
    /// the next `maintain()`.
    pub fn uninstall_rules(&mut self, id: &str) -> bool {
        self.engine.uninstall(id)
    }

    pub fn rule_batches(&self) -> Vec<(String, String)> {
        self.engine.batches()
    }

    /// Lookahead: "what would follow if this episode were true?" Extracts
    /// the episode's candidates (memoized under a hypothetical id, never
    /// colliding with real episodes), evaluates the goal under those
    /// temporary facts, and restores the memory untouched. Returns the
    /// goal bindings and the number of facts the assumption would add.
    pub fn what_if(
        &mut self,
        text: &str,
        goal: &str,
    ) -> Result<(Vec<String>, usize), Box<dyn std::error::Error>> {
        self.hyp_counter += 1;
        let episode = Episode {
            id: format!("hyp{}", self.hyp_counter),
            text: text.to_string(),
            ts: self.engine.now,
            speaker: None,
        };
        let candidates = self.extractor.extract(&episode);
        let now = self.engine.now;
        let extras: Vec<(String, Vec<Value>)> = candidates
            .iter()
            .map(|c| {
                (
                    "edge".to_string(),
                    vec![
                        self.engine.sym(&c.subj),
                        self.engine.sym(&c.pred),
                        self.engine.sym(&c.obj),
                        Value::Int(now),
                        Value::Int(i64::MAX),
                        Value::Int(now),
                    ],
                )
            })
            .collect();
        let refs: Vec<(&str, &[Value])> = extras
            .iter()
            .map(|(p, a)| (p.as_str(), a.as_slice()))
            .collect();
        let rows = self.engine.hypothetical(&refs, goal)?;
        Ok((rows, self.engine.last_hypothetical_facts))
    }

    /// Query `near` relevance facts for a session/entity pair.
    pub fn query_near(
        &self,
        session: crate::intern::Value,
        entity: crate::intern::Value,
    ) -> Vec<(Vec<crate::intern::Value>, crate::eval::Ann)> {
        self.engine.query("near", &[Some(session), Some(entity), None])
    }

    /// Demand-driven query (magic sets): answers without materializing the
    /// full fixpoint of the queried predicate. Runs an (idle-cheap)
    /// maintenance pass first so all-free adornments can alias fresh
    /// materialized relations instead of re-deriving closures.
    pub fn ask_deep(&mut self, goal: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let now = self.engine.now;
        self.maintain(now);
        self.engine.ask_deep(goal)
    }

    pub fn why(&self, fact: &str) -> String {
        match crate::ast::parse_program(&format!("{fact}.")) {
            Ok(clauses) if clauses.len() == 1 => {
                let head = &clauses[0].head;
                match self.engine.ground_values(&head.args) {
                    Some(args) => self.engine.why(&head.pred, &args),
                    None => format!("why: {fact} contains variables"),
                }
            }
            _ => format!("why: cannot parse {fact:?}"),
        }
    }

    pub fn episodes(&self) -> &[Episode] {
        &self.episodes
    }

    /// Extractor observability: (model calls, failures).
    pub fn extractor_stats(&self) -> (usize, usize) {
        self.extractor.stats()
    }

    /// Assemble the context window for a query mentioning `entities`:
    /// distilled facts first, verbatim provenance last, under budget. A
    /// leading "what changed in memory" section reports facts created since
    /// the last `maintain()` (capped).
    pub fn context(&self, entities: &[&str], budget_tokens: usize) -> String {
        let news: Vec<String> = self
            .engine
            .changes_from(self.last_turn_epoch)
            .iter()
            .take(20)
            .map(|(p, a)| self.engine.render_fact(p, a))
            .collect();
        assemble_context(&self.engine, &self.episodes, entities, budget_tokens, &news)
    }

    /// Query-driven context assembly via hybrid retrieval: BM25 over facts
    /// and episodes + entity-match boosting (a query naming an entity pulls
    /// that entity's facts and one-hop neighbors), budget-aware, distilled
    /// facts first and their provenance episodes last. This is the
    /// "selection, not extraction" answer to context bloat.
    pub fn context_for_query(&self, query: &str, budget_tokens: usize) -> String {
        let r = crate::retrieval::Retrieval::build(&self.engine, &self.episodes);
        let sel = r.select(query, budget_tokens);
        r.render(&sel)
    }
}

// ------------------------------------------------------ context assembler

/// Positional assembly (lost-in-the-middle mitigation): derived high-value
/// facts at the top of the window, verbatim source episodes at the bottom,
/// byte budget `tokens * 4` split 60/40.
pub fn assemble_context(
    engine: &Engine,
    episodes: &[Episode],
    entities: &[&str],
    budget_tokens: usize,
    news: &[String],
) -> String {
    let mut relevant: Vec<(Vec<Value>, Ann)> = Vec::new();
    for name in entities {
        let v = engine.sym_of(name);
        relevant.extend(engine.query("current", &[Some(v), None, None]));
    }
    relevant.sort_by(|a, b| b.1.conf.partial_cmp(&a.1.conf).unwrap_or(std::cmp::Ordering::Equal));

    let distilled_budget = (budget_tokens * 4 * 6 / 10).max(0);
    let mut distilled = String::new();
    let mut used_prov: Vec<String> = Vec::new();
    for (k, ann) in &relevant {
        let line = format!(
            "{} --{}--> {}   [conf {:.2}, prov {}]\n",
            engine.interner.display(&k[0]),
            engine.interner.display(&k[1]),
            engine.interner.display(&k[2]),
            ann.conf,
            ann.prov.iter().cloned().collect::<Vec<_>>().join(",")
        );
        if distilled.len() + line.len() > distilled_budget {
            break;
        }
        distilled.push_str(&line);
        used_prov.extend(ann.prov.iter().cloned());
    }

    let source_budget = (budget_tokens * 4 * 4 / 10).max(0);
    let mut sources = String::new();
    let mut used: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for ep in episodes {
        if !used_prov.iter().any(|p| p == &ep.id) {
            continue;
        }
        used.insert(&ep.id);
        let block = format!("[{}] {}\n", ep.id, ep.text);
        if sources.len() + block.len() > source_budget {
            break;
        }
        sources.push_str(&block);
    }
    let _ = used;

    let mut out = String::new();
    if !news.is_empty() {
        let _ = writeln!(out, "== new in memory since last turn ==");
        for line in news {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "== memory (distilled, highest confidence first) ==");
    out.push_str(&distilled);
    let _ = writeln!(out, "\n== source episodes (verbatim, from provenance) ==");
    out.push_str(&sources);
    out
}

impl Engine {
    /// Non-mutating symbol lookup for read-only paths.
    pub fn sym_of(&self, s: &str) -> Value {
        match self.interner.lookup(s) {
            Some(v) => Value::Sym(v),
            None => Value::Int(i64::MIN), // never matches: unknown entity
        }
    }

    /// Resolve a pattern to ground values for `why()` (None if vars remain).
    pub fn ground_values(&self, pat: &[Term]) -> Option<Vec<Value>> {
        pat.iter()
            .map(|t| match t {
                Term::Sym(s) => self.interner.lookup(s).map(Value::Sym),
                Term::Int(i) => Some(Value::Int(*i)),
                _ => None,
            })
            .collect()
    }
}

// ------------------------------------------------------------- persistence

impl<X: Extractor> AgentMemory<X> {
    /// Persist to a snapshot file: rules, clock, episodes (verbatim
    /// sources), escalation queue, and all base (EDB) facts with their
    /// annotations. Derived relations are NOT persisted — they are
    /// rebuildable projections, recomputed by `load()`.
    pub fn save(&self, path: &str) -> std::io::Result<()> {
        use crate::snapshot::{encode_provenance, escape, SNAPSHOT_MAGIC};
        use std::io::{BufWriter, Write};

        let file = std::fs::File::create(path)?;
        let mut out = BufWriter::new(file);
        writeln!(out, "{SNAPSHOT_MAGIC}")?;
        writeln!(out, "NOW\t{}", self.engine.now)?;
        writeln!(out, "RULES\t{}", escape(&self.extra_rules))?;
        for ep in &self.episodes {
            writeln!(
                out,
                "EP\t{}\t{}\t{}\t{}",
                escape(&ep.id),
                ep.ts,
                escape(ep.speaker.as_deref().unwrap_or("")),
                escape(&ep.text)
            )?;
        }
        for e in &self.escalations {
            writeln!(out, "ESC\t{}", escape(e))?;
        }
        for (pred, rel) in &self.engine.relations {
            // base facts only: skip predicates defined by rules (they are
            // either program facts re-declared from rules, or derived)
            if self.engine.clauses.iter().any(|c| c.head.pred == *pred) {
                continue;
            }
            for row in &rel.rows {
                let provenance = encode_provenance(row.fact.ann.prov.iter());
                let args = row
                    .key
                    .iter()
                    .map(|v| match v {
                        Value::Sym(s) => {
                            format!("s:{}", escape(self.engine.interner.resolve(*s)))
                        }
                        Value::Int(i) => format!("i:{i}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                writeln!(
                    out,
                    "FACT\t{}\t{}\t{}\t{}",
                    pred, row.fact.ann.conf, provenance, args
                )?;
            }
        }
        out.flush()
    }

    /// Load a snapshot into a fresh memory with the given extractor.
    /// Base facts are re-asserted with their annotations; derived
    /// relations are rebuilt by one maintenance run.
    pub fn load(extractor: X, path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        use crate::snapshot::{SnapshotReader, SnapshotRecord, SnapshotValue};
        use std::io::BufReader;

        let file = std::fs::File::open(path)?;
        let mut records = SnapshotReader::new(BufReader::new(file))?;
        let now = match records.next().transpose()? {
            Some(SnapshotRecord::Now(now)) => now,
            _ => return Err("snapshot is missing its NOW record".into()),
        };
        let rules = match records.next().transpose()? {
            Some(SnapshotRecord::Rules(rules)) => rules,
            _ => return Err("snapshot is missing its RULES record".into()),
        };
        let mut memory = AgentMemory::new(extractor, &rules)?;
        for record in records {
            match record? {
                SnapshotRecord::Episode {
                    id,
                    timestamp,
                    speaker,
                    text,
                } => memory.episodes.push(Episode {
                    id,
                    ts: timestamp,
                    speaker,
                    text,
                }),
                SnapshotRecord::Escalation(escalation) => {
                    memory.escalations.push(escalation);
                }
                SnapshotRecord::Fact {
                    predicate,
                    confidence,
                    provenance,
                    arguments,
                } => {
                    let resolved: Vec<Value> = arguments
                        .into_iter()
                        .map(|value| match value {
                            SnapshotValue::Symbol(name) => memory.engine.sym(&name),
                            SnapshotValue::Integer(integer) => Value::Int(integer),
                        })
                        .collect();
                    memory
                        .engine
                        .declare(&predicate, &resolved, Ann::base(confidence, provenance));
                }
                SnapshotRecord::Now(_) | SnapshotRecord::Rules(_) => {
                    return Err("snapshot contains a duplicate header record".into());
                }
            }
        }
        memory.episode_counter = memory.episodes.len() as u64;
        memory.engine.set_now(now);
        let _ = memory.engine.run();
        memory.last_turn_epoch = memory.engine.epoch();
        Ok(memory)
    }
}

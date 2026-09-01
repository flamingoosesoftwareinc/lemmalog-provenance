---
name: lemmalog
description: Store and retrieve workspace-scoped code facts with repository boundaries and provenance. Use for code questions, investigations, debugging, audits, planning, or other multi-step work involving architecture, behavior, dependencies, symbols, callers, references, diagnostics, hypotheses, evidence, or decisions.
---

# Lemmalog

Use Lemmalog as durable memory for code evidence. For every question about code, consult the resolved workspace store, then investigate with the normal code tools. Populate the store lazily as facts are verified; never scan the whole workspace only to initialize it. Lemmalog resolves `--workspace`, the nearest ancestor `.lemmalog` marker, or the target Git root fallback, in that order. A `.lemmalog` file contains `id = "stable-workspace-id"`. Stores live under `$XDG_DATA_HOME/lemmalog` (normally `~/.local/share/lemmalog`), not in the workspace.

## Install

If `lemmalog` is absent, install it once:

```sh
cargo install --git https://github.com/flamingoosesoftwareinc/lemmalog-provenance.git --bin lemmalog --locked
lemmalog skill install
```

The skill is embedded in the binary. `skill install` writes or updates `~/.agents/skills/lemmalog/SKILL.md`; use `--path <skills-directory>` for another agent or repository-local skills directory.

## When to use

- Any code search, explanation, or investigation: consult existing facts first and record durable findings as you verify them.
- Debugging or audits: track hypotheses, supporting or refuting evidence, and status changes.
- Planning or review: record decisions and the verified relationships that justify them.
- Specialist tools such as call graphs, coupling analysis, type comparison, and traces: preserve selected findings with their exact provenance.

## Use

1. Select the smallest read path:

   - Exact domain vocabulary known → query first:

   ```sh
   lemmalog query <repo-path> 'current("subject", "predicate", O)'
   ```

   - Vocabulary unknown → extract a small bounded set of concrete nouns, verbs or relationships, quoted errors, and symbol fragments from the question. Run basic lexical probes one at a time with conservative limits. Do not use the full natural-language question as one regex:

   ```sh
   lemmalog search <repo-path> 'timeout' --limit 10
   lemmalog search <repo-path> 'retry|backoff' --limit 10
   lemmalog search <repo-path> 'ClientError' --limit 10
   ```

   Expand probes only from subjects, predicates, or objects returned by the prior probe. Stop when enough terms exist for an exact `query`. If no useful probe can be formed, browse only the bounded stored vocabulary:

   ```sh
   lemmalog search <repo-path> '.*' --limit 50
   ```

   Search streams current stored base facts. It is not a workspace scan. Lowercase patterns ignore case; uppercase patterns preserve case. The default limit is 50. An empty search or query result means continue with normal code tools, not bootstrap a repository scan.

2. Confirm claims with source, Git, LSP, or another primary tool. Lemmalog memory is evidence to inspect, not authority.
3. Record concise, reusable relationships after confirmation:

   ```sh
   lemmalog observe <file-or-repo-path> 'subject --predicate--> object' \
     --provenance 'https://github.com/owner/repo/blob/commit/path#L20-L35'
   ```

   Pin source provenance to a commit and exact line range when possible. Prefer a clickable permalink produced by the source host, and verify it opens the cited source before recording it. Known formats differ:

   - GitHub: `https://github.com/<owner>/<repo>/blob/<full-sha>/<path>#L20-L35`
   - GitLab: `https://gitlab.com/<namespace>/<repo>/-/blob/<full-sha>/<path>#L20-35`
   - Bitbucket Cloud: `https://bitbucket.org/<workspace>/<repo>/src/<full-sha>/<path>#<filename>-20`; use its source view's copied URL for a multi-line selection.
   - Local source: use `file:///absolute/path` when opening the file is sufficient, or a verified editor deep link such as `vscode://file/<absolute-path>:20:1` when exact navigation is required.

   For another host or editor, use its **Copy permalink** action or official URL contract; do not invent a template. Never cite a mutable branch URL as permanent evidence. `--provenance` remains opaque: Lemmalog stores and propagates it without interpreting or validating its scheme. Repeat the argument for multiple sources. Avoid tabs and newlines because the evidence sidecar is line-oriented.
   Observations default to the containing repository. Use `--scope workspace` only for a fact that applies across the workspace. Queries see all workspace repositories by default; use `--scope repository` for the target repository plus shared facts, or `--scope workspace` for shared facts only.
   For an end-to-end join, query with one temporary Datalog rule, for example: `lemmalog query . 'flow(S) :- current("payments", "emits", E), current(E, "handled_by", S)'`.
4. Use discovered vocabulary in an exact query, then inspect the derivation before presenting, trusting, or citing the claim:

   ```sh
   lemmalog query <repo-path> 'current("payment_client", "retry_policy", O)'
   lemmalog why <repo-path> 'current(subject, predicate, object)'
   ```

   Validate the cited source when it is available. Present the relevant provenance URI with every user-facing claim taken from Lemmalog; do not present stored or derived facts as uncited knowledge.

## Shared vocabulary

These predicates are conventions, not enforced schema. Use them so later agents can query the same concepts:

```sh
lemmalog observe . 'h1 --hypothesis--> cache_causes_stale_reads' --provenance '<uri>'
lemmalog observe . 'h1 --status--> proposed' --provenance '<uri>'
lemmalog observe . 'h1 --evidence--> callgraph_finding_42' --provenance '<uri>'
lemmalog observe . 'd1 --decision--> invalidate_cache_on_write' --provenance '<uri>'
lemmalog query . 'current("h1", "status", S)'
lemmalog why . 'current(h1, status, proposed)'
```

Move hypotheses through `proposed`, `supported`, `refuted`, or `validated` only when new evidence justifies the status. Record decisions after choosing, so later agents can recover both the outcome and its provenance.

Use `describes` to define a project-specific predicate. The `evidence` predicate links facts in the model; `--provenance` cites the external source supporting an observation. Quote capitalized entity names in queries because bare capitalized words are variables.

Do not record guesses, transient implementation details, secrets, or facts without evidence. Do not access snapshot files directly or combine stores across repositories.

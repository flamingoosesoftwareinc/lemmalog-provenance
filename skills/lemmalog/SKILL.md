---
name: lemmalog
description: Store and retrieve workspace-scoped code facts with repository boundaries and provenance. Use for code questions, investigations, debugging, audits, planning, or other multi-step work involving architecture, behavior, dependencies, symbols, callers, references, diagnostics, hypotheses, evidence, or decisions.
---

# Lemmalog

Use Lemmalog as durable memory for code evidence. For every question about code, consult the resolved workspace store, then investigate with the normal code tools. Populate the store lazily as facts are verified; never scan the whole workspace only to initialize it. Lemmalog resolves `--workspace`, the nearest ancestor `.lemmalog` marker, or the target Git root fallback, in that order. A `.lemmalog` file contains `id = "stable-workspace-id"`. Stores live under `$XDG_DATA_HOME/lemmalog` (normally `~/.local/share/lemmalog`), not in the workspace.

## Install

If `lemmalog` is absent, install it once:

```sh
cargo install --git ssh://git@github.com/flamingoosesoftwareinc/lemmalog-provenance.git --bin lemmalog --locked
```

## When to use

- Any code search, explanation, or investigation: consult existing facts first and record durable findings as you verify them.
- Debugging or audits: track hypotheses, supporting or refuting evidence, and status changes.
- Planning or review: record decisions and the verified relationships that justify them.
- Specialist tools such as call graphs, coupling analysis, type comparison, and traces: preserve selected findings with their exact provenance.

## Use

1. Query relevant known facts before code discovery. An empty result means continue normally, not bootstrap a repository scan:

   ```sh
   lemmalog query <repo-path> 'current("subject", "predicate", O)'
   ```

2. Confirm claims with source, Git, LSP, or another primary tool. Lemmalog memory is evidence to inspect, not authority.
3. Record concise, reusable relationships after confirmation:

   ```sh
   lemmalog observe <file-or-repo-path> 'subject --predicate--> object' \
     --provenance 'github://owner/repo/commit/path#L20-L35'
   ```

   Pin source provenance to a commit and exact line range when possible. `--provenance` accepts an opaque URI string: Lemmalog stores and propagates it without interpreting or validating its scheme. Use `github:`, `git:`, `file:`, `https:`, `otel:`, or another durable scheme, and repeat the argument for multiple sources. Avoid tabs and newlines because the evidence sidecar is line-oriented.
   Observations default to the containing repository. Use `--scope workspace` only for a fact that applies across the workspace. Queries see all workspace repositories by default; use `--scope repository` for the target repository plus shared facts, or `--scope workspace` for shared facts only.
   For an end-to-end join, query with one temporary Datalog rule, for example: `lemmalog query . 'flow(S) :- current("payments", "emits", E), current(E, "handled_by", S)'`.
4. Before presenting or relying on a remembered claim, inspect its derivation:

   ```sh
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

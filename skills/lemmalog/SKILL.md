---
name: lemmalog
description: Store and retrieve repository-scoped code facts with provenance. Use whenever investigating, searching, or explaining code, including architecture, behavior, dependencies, symbols, callers, references, and diagnostics.
---

# Lemmalog

Use Lemmalog as durable memory for code evidence. For every question about code, consult the repository store, then investigate with the normal code tools. Populate the store lazily as facts are verified; never scan the whole repository only to initialize it. Lemmalog resolves any file or directory path to its Git repository and initializes that repository's store on the first observation. Stores live under `$XDG_DATA_HOME/lemmalog/repositories` (normally `~/.local/share/lemmalog/repositories`), not in the repository.

## Install

If `lemmalog` is absent, install it once:

```sh
cargo install --git ssh://git@github.com/flamingoosesoftwareinc/lemmalog-provenance.git --bin lemmalog --locked
```

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

   Pin source provenance to a commit and exact line range when possible. Use repeatable `--provenance` arguments for multiple sources. Provenance URIs are opaque, so another durable scheme is valid.
4. Before presenting or relying on a remembered claim, inspect its derivation:

   ```sh
   lemmalog why <repo-path> 'current(subject, predicate, object)'
   ```

   Validate the cited source when it is available. Present the relevant provenance URI with every user-facing claim taken from Lemmalog; do not present stored or derived facts as uncited knowledge.

Do not record guesses, transient implementation details, secrets, or facts without evidence. Do not access snapshot files directly or combine stores across repositories.

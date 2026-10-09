# Desktop architecture decisions

## Local Git access

- **Status:** Accepted
- **Date:** 2026-10-08

The desktop client will access local repositories through the embedded Rust
`gix` (gitoxide) library. It will not shell out to the user's `git` executable.
This avoids a runtime dependency on a separately installed Git binary and keeps
repository access behind an app-owned adapter.

Repository discovery, branch inspection, working-tree/index status, and text
diff extraction are read-only: they must not update the index, refs,
configuration, or worktree. UI state and review policy remain in the desktop
app; provider-neutral diff parsing and anchors remain in `cururu-core`. Remote
operations, staging, commits, and other Git mutations are out of scope unless
separately decided.

The app currently pins `gix` 0.85.0 with defaults disabled and the features
needed for local status. This version is used because newer candidates required
an unavailable `gix-dir` release in the configured package index during initial
integration. Review content passes through the core unified-diff parser before
rendering. Repositories can be opened through GPUI's native directory picker or
by passing a path on the command line. Text extraction has bounded inputs and
uses Cururu's current default diff limits (45,000 bytes/file, 180,000 total);
binary, symlink, and non-UTF-8 path support remains follow-up work.

## Local review-session persistence

- **Status:** Proposed; implementation is not approved until the open questions
  below are resolved
- **Date:** 2026-10-09

### Context

The desktop plan calls for local sessions that can be resumed and that retain
repository/base/head identity, comment drafts, review decisions, evaluation
state, and history. The storage must remain local-first; opening or reviewing a
repository must not upload code, comments, or session data. Credentials are
explicitly kept out of application data files and belong in the OS credential
store. Repository content can be re-read from the local worktree/object database,
so storing full diffs or source snippets should be avoided unless a later
offline-review requirement justifies it.

### Options considered

1. **JSON files with atomic replacement.** Simple and dependency-light. The
   sibling SQL Manager uses this pattern for a small connection-profile list
   (`config_dir/connections.json`, temporary file then rename) and stores
   passwords separately in the OS keyring. That is useful precedent for small
   preferences, but not evidence for a queryable session/history store with
   independently updated drafts and decisions.
2. **SQLite.** Relational queries, transactions, constraints, and schema
   migrations fit sessions, comments, decisions, and audit history. It is a
   mature single-user local store and can remain behind an app-owned adapter.
3. **Embedded key-value store (for example, redb).** Avoids SQL and offers
   transactional local storage, but adds a custom schema/indexing layer for
   querying sessions, changed files, drafts, and history.
4. **In-memory only.** No durable storage or migration burden, but loses drafts
   and review state when the app exits, so it does not satisfy the session
   journey.

### Recommendation (not yet accepted)

Use SQLite in the operating system's per-user application data directory,
behind a small `session_store` module and typed storage DTOs. Keep persistence
off the GPUI render path; use the GPUI background executor for synchronous
SQLite operations unless implementation evidence justifies an async driver.
Start with a single local writer, explicit schema migrations, and real-file
integration tests. Persist repository path/identity, base/head object IDs,
session mode/state, draft comments and anchors, user decisions, and bounded
evaluation metadata/history. Recompute diffs and line context from the
repository; do not persist credentials or duplicate source text by default.
Do not add cloud sync in this slice. A later sync feature must define conflict,
identity, encryption, and authorization semantics separately.

SQLite implementation crate (`rusqlite` versus an async driver), bundled versus
system SQLite, encryption-at-rest expectations for drafts/evaluation data,
multi-instance behavior, and retention/cleanup policy remain open. The
recommendation does not authorize adding a storage dependency or schema until
these choices are confirmed.

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

- **Status:** Accepted
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

### Decision

Use SQLite in the operating system's per-user application data directory via
`rusqlite` with bundled SQLite. Keep persistence behind a small `session_store`
module with typed storage records; run synchronous database work off the GPUI
render path on the GPUI background executor. Use explicit schema migrations
and real-file integration tests. The first slice persists local worktree
identity (repository path, branch, HEAD object ID) and last-opened time, and
offers an explicit “reopen last session” action rather than auto-opening a
repository at app launch.

The application will not add its own database encryption in this slice; local
data relies on OS account permissions and any full-disk encryption configured
by the user. Do not persist credentials, full source snapshots, or cloud-sync
data. Future draft comments, decisions, and bounded evaluation history may be
stored locally, but must not duplicate source text by default. Cloud sync and
its conflict, identity, encryption, and authorization semantics are separate
decisions. Multi-instance write behavior and retention/cleanup policy should be
revisited before adding durable draft/history records.

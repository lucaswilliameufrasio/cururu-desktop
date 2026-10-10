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

### Applying a suggested local change

- **Status:** Accepted for implementation
- **Date:** 2026-10-10

The first version may apply a suggested change to the local worktree only after
showing a preview, validating that the target still matches the reviewed base
and has no conflicting edits, and receiving an explicit user confirmation.
Generating or previewing a suggestion must never write files. Application must
not stage, commit, or push; those remain separate actions and are not part of
this workflow. If the app cannot establish that the target is still safe to
change, it must fail closed and preserve the user's current file.

The existing read-only behavior for repository discovery, Git status, and diff
extraction remains unchanged. This decision authorizes implementation of the
guarded local-edit workflow, not applying any change during development or
review of this repository.

### Shared review engine

- **Status:** Accepted for implementation
- **Date:** 2026-10-10

`cururu-core` remains a pure domain crate. Reusable asynchronous review
execution contracts live in `cururu-engine`, a library crate inside the
existing Cururu Git repository/workspace; this does not create another Git
repository or publish a crate. The desktop pins both packages to the same Git
revision so their shared core types remain identical. Provider configuration,
credentials, UI, and local-repository adapters remain owned by the desktop.

The initial extraction exposes the existing `ReviewAgent` contract, usage and
chunk result types, and sequential fail-closed chunk orchestration. It does not
yet make the desktop produce findings; connecting a real review provider and
workflow remains a follow-up slice. The initial engine commit is available at
`46c0d17`; the desktop manifest pins both crates there.

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

The workspace exposes “Review changes” and “Understand a review” as equally
prominent modes over the same open session. Switching modes changes only the
visible work focus; it does not reopen or clear the repository/PR, publish
anything, or discard local drafts.

### Local comment drafts

Drafts are user-authored and local-only until a future, separately authorized
publication flow exists. A draft anchors to a repository-relative path, a
positive line on the new side of the diff, and the HEAD OID observed when the
draft is saved. Removed-only lines cannot receive a draft in this slice. Saving
requires an explicit user action; changing selection or reopening the app never
deletes a draft. If the repository HEAD differs when drafts are loaded, show the
draft as stale and preserve its original anchor. Do not automatically relocate,
publish, or discard stale drafts. Store no source-line snapshots.

Drafts have no automatic expiry: they remain until the user explicitly deletes
them. Multiple app instances may use the local SQLite store; edits use a
revision check inside an immediate transaction, and a stale writer receives a
conflict instead of silently overwriting newer text. This is local concurrency
only, not cloud synchronization. When a conflict occurs, reload the repository
and draft before retrying. Revisit if drafts become synchronized or shared.

The draft editor uses `gpui-component` 0.3.0, pinned exactly because that
release targets GPUI 0.2.2; newer releases target a different GPUI generation.
Its Apache-2.0 license is compatible with this project's dual MIT/Apache-2.0
license. Default features stay disabled. This accepts a wider transitive
dependency graph in exchange for reusing the library's multiline native text
input, including platform text-input handling, instead of maintaining a custom
editor in the app.

## GitHub authentication

- **Status:** Accepted for implementation
- **Date:** 2026-10-09

Use a GitHub App, not an OAuth App, for the desktop's first remote integration.
Use the OAuth 2.0 Device Flow intended for native/desktop clients; do not embed
a client secret. Request expiring user access tokens with `offline_access`, and
store access/refresh tokens only in the operating-system credential store. If
that store is unavailable, fail closed rather than writing credentials to
SQLite or plaintext. The public GitHub App client ID is runtime/build
configuration and is not a secret.

Request only `Contents: read` and `Pull requests: read/write` for the approved
remote review capabilities; users still need the corresponding repository
access. Publishing remains a separate, explicit confirmation and is never
triggered by login or switching review modes. An optional `gh` fallback may be
used only after the user chooses it; do not copy its token into Cururu storage
or expose it in logs/output.

GitHub's current documentation states that device flow can be used by desktop
applications; GitHub App user tokens can be bounded by app permissions and
user access, and expiring tokens last 8 hours with refresh tokens lasting 6
months. Before the flow can run against GitHub, register/configure the app with
Device Flow enabled and supply its public client ID. App registration itself
is not part of this repository change.

### Read-only pull-request inspection

The initial remote review journey identifies a PR with an explicit
`owner/repository#number` supplied by the user. Do not infer the target PR
repository from a local `origin`: a fork's `origin` can differ from the PR base
repository. Use named GitHub API operations to read PR metadata, changed-file
patches, review submissions, inline review comments, and issue-conversation
comments. Preserve inline reply relationships by `in_reply_to_id`; a missing
current line with an original line is shown as an outdated location, not as a
resolved concern. Follow only HTTPS pagination links on `api.github.com`, use
bounded pages/responses/diffs, and fail closed when a patch is missing or
incomplete. This remote review slice is read-only and does not invoke the
evaluator or persist remote content. When the user selects an inline comment,
fetch its source file on demand at `original_commit_id` using the GitHub
Contents API; cap files at 256 KiB, decode only base64 UTF-8 text, show a small
bounded excerpt, and retain it only in memory. Do not fetch source files for all
comments automatically. Publication remains a separate, explicitly confirmed
feature; revisit repository discovery when a multi-repository chooser is
designed.

### Review understanding evaluators

Both a user-configured OpenAI-compatible chat-completions provider and Jev are
required for the “Understand a review” AI action. Run them concurrently on the
same explicitly selected thread and bounded evidence; do not silently fall
back to only one provider. OpenAI-compatible supplies the structured
explanation and response draft; Jev supplies its typed defect-probability and
severity second opinion. If either call fails or times out, show no combined
judgment. The result is informational only: it never filters findings, changes
review state, edits code, or publishes comments.

The API keys belong only in the OS keyring. Non-secret endpoint/model settings
and the explicit local `owner/repository` authorization belong in the local
SQLite store. Authorization is scoped to the exact canonical GitHub repository
and a fingerprint of both provider endpoints/models; changing either provider
configuration requires fresh approval. Before sending any comment/source, the
UI must identify that both providers will receive the same bounded context and
require the repository authorization. Each analysis has one 2-minute deadline,
no internal retries, and fail-closed behavior. Evidence sent per selected
thread is capped at 8,000 characters; remote content remains untrusted data and
cannot trigger tools or actions.

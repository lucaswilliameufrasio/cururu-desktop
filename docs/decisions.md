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

The initial implementation accepts only a single-line replacement on a valid
changed-line anchor. It previews the exact unified diff, rejects symbolic-link
path components, and requires both the reviewed worktree diff and complete file
contents to remain unchanged between preview and confirmation. After the final
recheck, write through a same-directory temporary file and atomic rename while
preserving file permissions. Applying a suggestion invalidates the prior local
review findings; run a new review before applying another suggestion.

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
chunk result types, sequential fail-closed chunk orchestration, and the
canonical Cururu review prompt. The desktop consumes these contracts for its
local OpenAI-compatible reviewer; both crates are pinned to engine commit
`90b8869`.

### Local code review and provider consent

- **Status:** Accepted for implementation
- **Date:** 2026-10-10

The first local reviewer uses Cururu's shared OpenAI-compatible engine and
canonical prompt. It sends only bounded changed-file unified diffs and paths,
never a repository snapshot. The endpoint and model are app-local settings;
the API key lives in a dedicated OS-keyring entry, separate from evaluator
credentials. Endpoints must use HTTPS.

Consent is separate from configuring credentials and separate from the dual
evaluator consent. It is stored per canonical local checkout path and
configuration fingerprint. Saving settings or authorizing sends no code. A
separate user action starts a review and displays the repository, destination,
file count, and diff-size limits before sending. Missing or mismatched local
consent, stale worktree state, invalid model anchors, or a failed chunk aborts
the run without showing partial findings. Results remain informational; this
decision does not authorize applying suggestions, staging, committing, or
publishing them.

Keep reviewer policy and prompt behavior in Cururu rather than copying review
heuristics into the desktop. Revisit consent fingerprint versioning if the
outbound evidence boundary or destination semantics change.

## Unified and side-by-side diff presentation

- **Status:** Accepted for implementation from the approved desktop plan
- **Date:** 2026-10-15

Both layouts use the same parsed diff lines and canonical line selection. The
side-by-side view pairs removals/additions within a contiguous change block in
one hunk, not by text similarity. Unpaired lines have an empty opposite cell;
context lines appear on both sides. A click on either populated cell selects
the original unified line, preserving old/new anchors for threads and drafts.
Layout switching only changes presentation and scroll position; it never clears
drafts, publishes, runs analysis, or changes files. Cache row/index mappings per
file, and keep one virtualized list for synchronized vertical scrolling rather
than maintaining two independently scrolling panes. Revisit alignment heuristics
only with real review examples; do not imply semantic equivalence of paired lines.

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

The MVP also supports an explicitly selected `gh` CLI fallback. The app may
read the token into memory for the current app session only; it must not persist
or log the token, and disconnecting this app must not sign the user out of `gh`.
Device Flow remains the primary path. Validate the requested GitHub App scopes
and revocation behavior before live use.

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

### Explicit inline-thread replies

- **Status:** Accepted for implementation
- **Date:** 2026-10-13

The first GitHub publication slice supports one reply to a selected inline
review-thread comment. It does not publish a batch review, a review summary, or
a general issue-conversation comment. Drafting and previewing are local-only.
Before publication, show the exact pull request URL and title, confirmed head,
target comment and author, and complete reply body; the user must then choose a
separate **Confirm and publish this reply** action. Changing review modes or
selecting another thread never publishes.

Immediately before the write, reload the pull request and fail closed unless it
is still open, its head SHA matches the preview, and the target comment still
exists. Send one request to GitHub's pull-request review-comment reply endpoint;
do not retry automatically. If the outcome cannot be confirmed, tell the user
to refresh the pull request before deciding whether to retry, since the server
may have accepted a request whose response was lost. During revalidation, refuse
to publish when the same reply body already exists in the selected thread. The
UI also keeps the exact successful reply visible and blocks an accidental
identical repeat in that thread. This workflow does not modify local files,
stage, commit, or push.

### New inline comments and batch comment reviews

- **Status:** Accepted for implementation from the approved publication journey
- **Date:** 2026-10-15

User-authored comments on new-side diff lines can be queued in memory and sent
individually or together as one GitHub `COMMENT` review. Before any write, preview
the PR, base/head identities, exact anchors, and full bodies; require a separate
confirmation. Require a user-authored summary (GitHub requires a body for the
`COMMENT` event); include it unchanged in the preview and request. Revalidate
that the PR is open, both SHAs are unchanged, every
anchor still exists, and no identical comment already exists at the same anchor.
Send one create-review request bound to the confirmed head, without redirects
or automatic retries. Preserve drafts after errors and require refresh after an
ambiguous outcome; never automatically resend. Mode/layout changes do not discard
drafts. A draft remains scoped to its exact PR/head. Approve/request-changes votes,
general conversation comments and applying remote
suggestions remain distinct slices; this feature neither runs code nor edits a
checkout. No live GitHub publication is authorized by implementation approval.

An explicit local save persists the bounded draft queue and summary in schema 6
of the existing SQLite database; startup restores it without sending any content.
Only PR/base/head identities, paths/lines and user-authored bodies are stored, not
credentials or source snapshots. Saves use an immediate transaction and an
optimistic revision over the queue to reject stale multi-instance writers.
Database work for the save runs on the background executor. Edits/removals and
successful publication change the in-memory queue; saving again updates the
durable snapshot. If the stored snapshot includes a previously published draft,
publication revalidation must still reject a duplicate rather than resending it.
Invalid stored data fails closed, is not silently deleted, and disables writes
from that app instance. Revisit whole-queue revisions if independent multi-instance
editing of separate PRs becomes necessary.

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

Evaluator rollout starts in observe-only mode. It must not filter or suppress
findings until a labeled evaluation set has been measured and thresholds and
rollout criteria have been separately calibrated.

## Local test and analyzer execution

- **Status:** Linux Bubblewrap and macOS Seatbelt validated in native CI; interactive validation pending
- **Date:** 2026-10-14

Running a test or analyzer is a separate, explicit action from reviewing code.
For each run, show the exact executable and arguments, repository and working
directory, reviewed HEAD/diff identity, and whether isolation is active. Require
fresh confirmation for every command; do not infer approval from a previous
run. Launch the executable directly without implicit shell parsing or
interpolation. Do not inject application/provider credentials or automatically
load repository environment files. Do not persist command output.

Use an available OS sandbox when it can be established for that run. If no
sandbox is available, clearly warn that the command will run with the user's OS
permissions and require a separate confirmation for that unsandboxed run. Bound
runtime and captured output, show exit status and bounded output, and offer
cancellation. Cancellation must terminate the child process and reap it; do not
retry commands. Remote PR content remains untrusted and never authorizes a run.

The first implementation uses Bubblewrap on Linux when a namespace and the
required checkout/home mounts pass preflight. The checkout is read-only, the
network namespace is unshared, temporary directories are isolated, and the
user's home/credential stores are hidden; only Rust toolchain and Cargo cache
paths needed by supported commands are mounted read-only. macOS uses the system
Seatbelt `sandbox-exec` CLI with a deny-by-default profile: the checkout and
allowlisted Rust toolchain/cache paths are read-only, networking is denied, and
writes are limited to the private per-run temporary directory plus required
system device nodes. The CLI is deprecated
by Apple; the app fails closed to the separately confirmed unsandboxed path if
the binary or sandbox profile preflight is unavailable. Native macOS CI must
validate the backend before it is treated as complete. Each run is capped at
10 minutes and captures at most 32 KiB per output stream in memory; cancellation
sends SIGTERM to the process group and escalates to SIGKILL after two seconds.
The app revalidates repository identity, HEAD, and diff before and after the run,
discarding output if the reviewed checkout becomes stale. These limits do not
authorize running repository-provided code during development or review of this
repository.

## Initial desktop distribution targets

- **Status:** Accepted for packaging preparation, not release
- **Date:** 2026-10-13

The initial architecture matrix is Linux x86_64, Linux ARM64, and macOS Apple
Silicon (ARM64). Prepare Linux AppImage and macOS `.app`/`.dmg` artifacts, but
verify the packager/runtime supports the Linux ARM64 target before treating that
artifact as available. The CI matrix now uses native x86_64, ARM64, and Apple
Silicon runners, with `cargo-packager` 0.11.8 building the selected packages.
The packager's AppImage implementation has ARM64-specific AppRun, linuxdeploy,
and plugin assets; the native CI build still needs to pass before claiming the
ARM64 artifact is validated. Linux packaging sets `NO_STRIP=1` because
linuxdeploy's bundled strip cannot read Ubuntu 24.04 `.relr.dyn` sections.
Windows, macOS Intel, and Linux packages beyond AppImage remain out of this
initial matrix. CI packaging is validation-only: no signing, artifact upload,
release, deployment, or publication is configured or authorized.

## Rust toolchain

- **Status:** Accepted
- **Date:** 2026-10-14

Set the desktop's minimum supported Rust version to 1.99 and pin CI to Rust
1.99.0, matching the toolchain used for ongoing Cururu desktop development.

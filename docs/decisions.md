# Desktop architecture decisions

## Per-finding human decisions in immutable saved reviews

Extend the explicit local archive with decisions for visible and suppressed
findings. Annotate an already-saved record, not the live review. Each decision
targets an ordinal plus the complete finding hash and creates a new immutable
record linked to its parent. Concurrent annotations branch rather than overwrite.
Repeated annotation of the same parent/choice is idempotent. Stale findings can
be annotated as historical evidence, but annotations never restore apply/publish
permissions, suppress live findings or calibrate evaluators. No free-text notes.

Keep SQLite schema 9 and legacy payload hashes by omitting empty optional fields.
Version 2 carries validated parent/decisions; the parent must exist in the same
checkout archive at annotation time. Deleting a parent later does not delete its
descendants. Existing 200-record/64,000-byte limits apply to annotations too;
overflow rejects without replacing history.

## Automatic HEAD context remains an explicit proposal

Following accepted principal ADR 008 and the user's confirmed pure-rule scope,
reuse engine auto-context defaults and candidate matching. Desktop candidates
are changed paths present as regular files in the loaded HEAD tree, excluding
already-declared paths. Names-only discovery may inspect tree entries/OIDs but
does not load blob content. Show revision and source per path; a separate Load
reads exactly those pinned blobs, never the current working files as a fallback.
New/unborn paths without a HEAD version have no base candidate. No network read.

Keep Discover → Load → Include (or diff-only). Require fresh HEAD/manifest checks
before content loading and final review/suggestion guards. Apply desktop file,
count and aggregate ceilings on top of shared settings. Unlike CLI's best-effort
truncation/skipping, desktop rejects oversized/non-text/sensitive candidates or
count excess; no silently incomplete evidence under a preview approval. Automatic
max_bytes/max_files include explicitly declared context, matching CLI accounting.
Configuration enables proposals, not filesystem or provider access. Revisit for
another base revision only with explicit provenance and approval semantics.

## Explicit history for local review results and suppressions

SQLite schema 9 adds a separate immutable local-review archive, with explicit
Save/Load/Inspect/Delete actions and optional whole-review human annotations.
Persist normalized findings, policy suppressions with reasons, discarded counts,
usage/model and head/diff/provider/context/base fingerprints. Use a canonical
checkout hash as the repository key rather than copying its absolute path into
the payload. Do not persist raw prompts, source snapshots, credentials, approved
context files or shared-base documents: an explicit result DTO makes these
in-memory capability types unavailable to serialization.

Finding explanations/suggestions can quote code; warn before Save. Revalidate
the current UI/provider/context/base identity and a fresh local snapshot before
saving. Record identity includes the human annotation; edits create another
record instead of replacing history. These annotations are subjective and do
not authorize evaluator filtering or constitute calibrated labels.

Bound this archive independently to 200 records, 64,000 bytes/record and 20 per
page. Reject excess/corruption rather than truncate or evict. Inspect one result
at a time. Archived results are plain read-only text, without apply/publish
controls or restoration into live analysis/consent state. Loaded identity matches
are not proof of current filesystem freshness. Deletion is explicit and does
not promise secure erasure from SQLite/backups. Migration 8→9 preserves the
second-opinion archive, publication preferences and queued drafts.

## Explicit names-only context glob discovery

Extend exact context paths with the core's glob semantics. Resolve globs only
against tracked, unconflicted regular-file Git index entries, never a recursive
filesystem walk. Compile glob sets with the same defaults used by core matching;
tests compare matches to `cururu_core::context_path_matches`. Exact declarations
can name untracked files explicitly. Bound discovery to 200,000 index entries
and 32 resolved files; reject invalid/no-match/sensitive/non-file/excess results
instead of skipping or truncating. Deduplicate overlapping paths in declaration
order, keeping the first label.

Discovery is a separate user action that produces only a names manifest. Show
the resolved list, require an explicit Load to read those exact files, then
require inclusion or an explicit diff-only choice. Neither discovery nor Load
authorizes transmission. Memory-only enum states distinguish each phase. Before
reading, recompute names and compare the complete manifest, scope and labels;
new matches must not be opened under an earlier approval. Content/revision guards
continue to use safe descriptor-relative reads and shared-base approval.
Automatic context discovery remains unsupported: configuration cannot initiate
these actions. This supersedes the earlier exact-path-only limitation, not the
consent, size, credential or execution boundaries.

## Pinned shared-base configuration with explicit approval

Use shared schema and pure TOML composition from engine pin af32737 (ADR 007 in
Cururu). Runtime interpretation, fetch, filesystem, credentials and permissions
remain desktop-owned. A declaration is not authorization: pending bases block
AI review until an explicit authenticated github.com GET, effective-policy
preview and memory-only approval. Fetch exactly one file at a full pinned SHA;
reject nested bases, non-file responses, invalid paths/identity, unsupported
settings or size excess. Cap source files at 64 KiB and composed text at 128 KiB.

Apply canonical base/local precedence first, then local desktop ceilings and
permission boundaries. Never adopt provider destinations/model/key, evaluator
permissions, or execute/fetch context declarations from the base. Context paths
refer to the consumer checkout and still require separate local preview and
inclusion. Show the effective supported fields and provenance, not raw TOML
which may contain credentials. Opaque documents have redacted Debug and no
serialization/persistence contract; approval and pinned bytes are memory-only.

Bind approval to canonical checkout, exact local document, pinned source and
base content fingerprint. Review identity records the independent base hash;
manual diff/command identity retains the raw local config hash, so approving a
base never authorizes or changes an approved command. Pre/postflight and local
suggestions reconstruct effective policy using cached approved bytes after
rechecking local config/root; no automatic network access. A changed/revoked
approval invalidates analysis/context choices, not manual diff/drafts. Revisit
multi-level bases or caching only with explicit provenance/consent semantics.

## Import GitHub suggestions without implicit commit/push

Support exactly one plain single-line suggestion block on a current-head,
right-side, non-outdated GitHub inline comment. Reject offsets, empty/deletion,
multiline and ambiguous/nested fences instead of guessing ranges. Use loaded
source at the same head and exact changed-line anchors. The user selects a local
checkout, which must already be at that head; do not clone, checkout, fetch,
stage, commit or push. Require the local target to match the full reviewed source
and show a diff before a separate application confirmation.

Before application, read fresh PR metadata/files/comments with the existing
bounded client and 30-second timeout, compare reference/base/head/comment/anchor,
then recheck the local snapshot and exact source through the existing safe-write
boundary. No remote POST/PATCH/PUT/DELETE is used. This is a preflight, not a
distributed lock on GitHub: a later remote update does not authorize a push.
Busy preparation/application blocks checkout/PR navigation. A source already
changed by the user is never overwritten; repeated application fails closed.

A separate explicit link opens the validated repository/PR/comment URL at GitHub
for its hosted application/commit flow, without using untrusted `html_url` as an
arbitrary browser target. Native remote commit creation is not implemented by
this import flow; application, commit and push remain distinct actions.

## Explicit local repository context and focus

The desktop consumes engine pin dc7a270 and its bounded `ReviewContext` contract.
This extends the earlier restriction-only configuration slice for local context
and focus only. Shared-base fetching, globs, automatic context discovery and
analyzer ingestion remain unsupported/fail-closed. Accept exact configured paths
and `policy.focus`; parsing must not open referenced files. List declarations,
require a separate local Load action, show the text, then require inclusion or
an explicit diff-only choice. Do not silently omit declared evidence. Choices
are memory-only and bound to canonical checkout plus diff/configuration hash.
Provider consent is separate and its fingerprint is versioned for this extension.

Host reads use descriptor-relative `openat` with no-follow flags on every path
component, bounded nonblocking reads and regular-file/link checks. Block known
credential/configuration paths, traversal, globs, symlinks, hardlinks, special
files, binary text and excesses; do not truncate. This is not a general secret
scanner: warn that document contents may be private and require visual inspection
before inclusion. Limits intersect engine ceilings (32 files, 16kB/file, 64kB
content total); context is repeated for each of at most four diff chunks.

Keep approved context in the request/result, not SQLite. Re-read approved files
in pre/postflight and before preparing/applying suggestions. Compare evidence
hashes even when Git ignores the document. Context stays separate from canonical
diff anchors and the system prompt: the shared adapter sends it as JSON user
data with an untrusted-evidence warning. Existing empty-context wire behavior is
preserved. Skills referenced as context are not executable agent instructions.

## Personal publication preview preference

SQLite schema 8 stores an app-local individual/batch preference. Batch remains
the default to preserve existing behavior. This only changes the primary preview
action (next matching queued comment vs all matching queued comments). Both paths
remain available. It grants no permission, triggers no publication, never drops
drafts and cannot be set by repository configuration. Changing a preference
invalidates an existing preview confirmation, but does not clear a failed or
uncertain publication state that requires refresh. Persist changes off the UI
thread; failures retain the previous preference. No OAuth/credential data is
stored here. Migration tests preserve the existing evaluation archive and drafts.

## Explicit local evaluation archive

SQLite schema 7 adds an immutable, explicitly saved second-opinion archive,
scoped to GitHub repository/PR. Identity includes base/head/source SHAs, comment
ID, a hash of the bounded evidence, provider configuration and rubric version.
Freshness checks also compare the selected comment with the canonical loaded
thread, preventing edited comments or changed revisions from accepting an old
in-flight result. Provider/rubric changes invalidate existing local consent.

Saving is opt-in and local only, with an optional human assessment (unreviewed,
agree, disagree, needs more evidence). A different assessment creates another
immutable record, not an overwrite. These are subjective annotations, not a
calibrated dataset and never enable filtering. Reading the archive does not
restore live analysis, permissions, reply drafts or publication confirmations.
Records not matching current evidence are labeled stale/unverified.

Store provider explanations, model IDs and reported usage, but not provider keys,
full inputs or source snapshots. Explanations may quote code/comments; disclose
that before saving. Cap records at 64,000 bytes, 200 records globally and 20 per
page. Reject excess rather than truncate, silently evict, or overwrite. Deletion
is an explicit local action; it is not a secure-erasure guarantee for SQLite,
backups or filesystem snapshots. Real temporary SQLite tests cover migration,
reopening, isolation, idempotency, corruption, limits and explicit deletion.

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
`c4428b1` (including the shared configuration schema).

### Shared repository configuration boundary

- **Status:** Accepted by the user
- **Date:** 2026-10-16

Use `cururu-engine::config` for the versioned `.cururu.toml` shape/parser and
portable validation. Environment, credentials, runtime merging, SCM, filesystem
access and permissions stay in hosts (Cururu ADR 006). Do not copy the CLI schema
or import all `AppConfig`. The initial desktop interpretation is restriction-only:
`review.max_diff_bytes`, `chunk_bytes`, `ignore`, and `policy.minimum_confidence`,
`max_findings`, `allowed_severities`, `suggested_changes`. Repository limits are
intersected with desktop ceilings; provider fields never change the locally
configured destination/model/key. `[evaluator]` is rejected, not activated.

Read only the regular, non-symlink `.cururu.toml` at the selected root, bounded to
64 KiB, on the background repository-snapshot path. Do not load shared bases,
context files, analyzers, prompts, or environment files implicitly. Unsupported
review/policy settings and `[config]`, `[context]`, `[analysis]` fail closed for AI
review while manual diff/drafts remain available. `[summary]` is not applied.
Surface this partial support rather than pretending full CLI configuration is
implemented. The config file's own patch is excluded from outbound evidence.

Hash the raw config into the snapshot identity, including when Git ignores it,
and revalidate before/after review. Repository restrictions cannot replace local
consent. Keep valid policy-suppressed findings and their reasons visible for
audit; this is not evaluator filtering, which remains observe-only. Extending
outbound context/focus later requires an explicit bounded evidence/consent design.

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

The `gh` fallback is now an explicit **Use existing gh login (memory only)**
action, never automatic after Device Flow failure. Invoke only installed `gh`
from fixed system/Homebrew/user-local installation directories, not repository
PATH entries; run `auth token --hostname github.com` without a shell, with a
cleared environment (only home/config/keyring locators are restored), no stdin,
discarded stderr, bounded 16 KiB nonblocking stdout and a five-second deadline.
Terminate/reap its process group even when output pipes are held by descendants.
Do not inherit GH_TOKEN/GITHUB_TOKEN or app/provider credentials. No live token
is read during development tests; synthetic executables exercise the boundary.

Session credentials distinguish Device Flow from `gh` at the type boundary:
only the Device Flow payload can enter `CredentialStore`. gh token expiration
is not known locally; GitHub remains authoritative and the user must disconnect
and select the fallback again if rejected. Disconnecting a gh-backed session
only clears app memory, without touching Cururu's OS-keyring entries or running
`gh auth logout`. A saved Device Flow grant is not overwritten by the fallback.

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

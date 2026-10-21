# Cururu Desktop

**Second opinion for a local finding** is available for live provider findings
(including policy-suppressed findings) and loaded static findings, in Review
changes or Understand a review. Configure both evaluators in Understand a review,
select **Preview second opinion**, inspect the exact finding JSON/full selected
file diff and both destinations, then choose **Confirm send to both**.
This is one-shot consent for that checkout/finding/configuration, separate from
GitHub and local-review authorization. Preview never reads the keyring or sends.
No extra source/context files are collected; the full selected diff must fit the
existing 8,000-character evidence budget, otherwise the preview fails without
truncation. Local checkout identity is a typed scope hash, not a fabricated PR.

Both evaluators must succeed within the existing two-minute deadline. Fresh
checkout/artifact/settings checks bracket the call; stale completions and partial
failures are discarded without automatic retry. Results are observe-only: no
filtering, commands, apply or publication. Save explicitly to create an immutable
local-history source snapshot and linked per-finding opinion, atomically under
the existing 200-record/64,000-byte limits. Opinion prose may quote private code;
inspect before saving. Only normalized findings/opinions, usage and fingerprints
are archived, not full evaluator inputs/context or credentials. Loading history
never restores a live opinion or consent. Manual assessments are not calibration.

Loaded static evidence is local-only by default. A separate **Include with the
next AI review** button approves normalized diagnostics/tool claims (up to 64,000
bytes repeated across at most four chunks), not raw SARIF. Inspect private content
first. Diff-only revokes this inclusion; including later is shown as a separate
choice. Include/exclude clears active results/suggestions, never saved history.
Provider reviews merge the original anchored tool findings through shared core
rules and retain provenance/suppression reasons. Request, incremental, history
and suggestion guards bind the artifact/report hash and revalidate fresh files.
Changing an artifact blocks sending/saving/applying stale results. Saved history
never restores inclusion or permissions. Evaluators remain observe-only.

Explicit `review.language`, `tone`, `technical_level` and `suggestion_detail` are
bounded optional preferences, shown in the context preview and sent only after
inclusion. They travel as JSON user data, never system instructions, and are
covered by context/review/suggestion/incremental fingerprints. Diff-only excludes
them as well. No defaults or provider/permission settings are copied from CLI.
Valid `review.comment_mode` is an advisory hint only; personal publication controls
still choose the actual workflow. `recommendations = false` is accepted; true
remains blocked because it must not authorize extra provider calls from the repo.

**Local static analysis** supports explicit `[analysis] enabled = true` with an
exact `manifest` path and optional exact `sarif_paths`. Read the manifest first
to inspect its tool claims and referenced names, then explicitly load SARIF.
Findings use the shared parser/core model and can navigate to valid changed-line
anchors after checkout/artifact revalidation, without an AI key or network access.
Loading never executes a command, sends evidence, applies or publishes anything.

The manifest needs schema 1, `commit_sha` equal to current HEAD and
`desktop_diff_fingerprint` copied from the panel. Its `tools` entries use
`name`, `status` (passed/succeeded/failed/not_run), optional `exit_code`, `message`
and `sarif_path`. These are untrusted claims, not proof a tool ran. Keep artifact
files in a Git-ignored directory (such as an ignored build directory), outside
the working-tree diff; otherwise the manifest fingerprint becomes self-referential.
Limits: 64,000-byte manifest / 8 tools, 8 exact SARIF files of 128,000 bytes,
100 candidates and a 64,000-byte normalized report. Unsafe/nonregular files,
excess and stale/missing bindings fail rather than truncate. Globs, unbound
standalone SARIF, remote check-run fetching and disabling current-revision checks
remain unsupported. Reports stay in memory unless explicitly saved from Local
review history. Static snapshots preserve normalized findings/tool claims and
artifact hashes, not source artifact bytes, provider tokens or permissions.
Saving revalidates the checkout/artifacts. Saved static findings can receive
immutable human annotations, but are not restored into active analysis or used
as provider incremental hits. No implicit merge or send with AI reviews occurs.
Existing approved/sandboxed commands remain separate.

`policy.synthesis` uses the shared core's candidate deduplication. `policy.fail_on`
uses its severity quality gate and shows an informational passed/failed/disabled
status for retained findings, also preserved in newly saved review snapshots.
This status never publishes, suppresses findings, applies changes or blocks human
discussion automatically. Legacy archives without a threshold show it as unknown.
`policy.incremental = true` skips a manually requested provider review when an
explicitly saved record matches the exact checkout/HEAD/diff/provider/context/base
and included static-evidence inputs. The panel shows its ID without reading the keyring or calling the provider;
it does not restore findings or apply/publish permissions. A separate explicit
send-again button bypasses this lookup, never consent/freshness/limits. Only saved
records participate. Storage corruption fails closed rather than silently sending.

Explicit `review.profile` supports balanced/strict/security/minimal through the
same pure catalog as CLI, followed by explicit policy overrides and desktop
ceilings. With no profile, desktop defaults do not change. An explicit focus
replaces profile focus; a security focus still requires a separate context
choice. Profiles never grant provider/command/apply/publish permissions.

**Action receipts** keep observed suggestion applications/imports, GitHub reviews
and replies, and command completions in memory until you explicitly save them.
SQLite schema 10 stores only typed kind/outcome, time/revision, scope/input/result
hashes, target count and an optional remote ID. No command arguments/output,
comment bodies, file paths, replacement code, tokens or URLs are saved. Inspect,
save, discard and delete never repeat an action. Unknown write outcomes remain
**unconfirmed**, not proof that nothing happened or permission to retry.
At most 20 unsaved receipts and 500 durable receipts (4096 bytes each, 20/page)
are retained; excess is rejected with a warning, never silently evicted. This is
not a crash-safe or complete audit log. Database errors keep pending receipts in
memory; an already-performed action is not retried because saving failed. Import
receipts are scoped to the selected local checkout, not the remote PR. Deleting
records is explicit and does not guarantee secure erasure from SQLite/backups.

**Local review history** explicitly saves completed findings, policy-suppressed
findings with reasons, usage and an optional whole-review human assessment.
Records include revision/diff/provider/context/base fingerprints, not credentials,
raw inputs or context/shared-base documents. Finding prose may quote code; check
it before saving. Loading history is read-only: it does not restore live findings,
approval, apply or publication state. Detailed records are opened individually;
matching loaded identity is not a fresh filesystem validation. Limits are 200
local review records, 64,000 bytes each, 20 per page. Assessment changes create
new immutable records; nothing is silently overwritten/pruned. Delete explicitly.
This archive is separate from the second-opinion archive and does not enable
automatic filtering or evaluator calibration.

Inspect a saved review and select an individual finding (including suppressed
findings) to save an agreement/disagreement/needs-evidence decision. Each choice
creates another immutable record linked to the saved parent; it does not edit
the parent or live analysis. Annotations also work for historical/stale reviews,
but cannot restore application/publication permissions or enable filtering.
Concurrent choices branch rather than overwrite. Decisions use the exact finding
hash and ordinal; deleting a parent does not cascade to its descendants. These
records share the archive's existing limits and explicit deletion behavior.

Pinned GitHub shared configuration is supported via `[config]` (`base =
"owner/repository"`, full 40-character `base_ref`, repository-relative
`base_path`). Opening a checkout never fetches it. Explicitly fetch with your
GitHub session, inspect the effective policy, then approve it in memory. Sources
are limited to 64 KiB, composed configuration to 128 KiB; nested bases are
rejected. Base/local composition uses the same engine function as the CLI.
Local overrides and cumulative arrays are preserved, then desktop ceilings and
unsupported-field checks apply. Provider settings never change your local
destination/model/key/consent. Referenced context is not fetched or read by base
approval: its own preview/inclusion step remains separate. Raw base documents
are redacted from debug output and are not stored in SQLite. Review pre/postflight
and suggestion guards reuse the approved pinned bytes, checking the exact local
configuration without an automatic remote fetch. Clearing approval blocks AI
review again. Changing a pointer or local document requires fresh approval.

A plain single-line GitHub `suggestion` block on a current-head right-side inline
comment can be imported into an explicitly chosen checkout. Load its source,
choose a checkout already at the exact PR head, inspect the local diff, then
confirm **Apply this exact preview locally**. The target must exactly match the
reviewed GitHub source. A fresh read of the PR/comment plus local HEAD/diff/file
checks blocks stale edits. This does not stage, commit, push or write to GitHub.
Offsets, multiline/deletion/ambiguous blocks and outdated anchors are rejected.
The separate **Open suggestion on GitHub** action hands remote application/commit
to GitHub's own UI; it does not apply the suggestion automatically.

The GitHub composer remembers a personal **Individual comments / Batch review**
preview preference locally. This changes only the primary preview action: both
paths still require explicit confirmation, and switching never publishes or
discards drafts.

Second opinions can be explicitly saved in the **Local evaluation archive**, with
an optional manual assessment. Loading/deleting records is local only. Archived
results are never restored as live analysis, consent or publication state, and
are labeled stale/unverified unless their revision and evidence identity match.
Provider prose can contain quoted code; full inputs and keys are not archived.
Limits: 200 records, 64,000 bytes each, 20 per page; nothing is silently pruned.
Manual assessments do not enable automatic filtering or calibrated thresholds.

Cururu Desktop is a native code-review client built on GPUI and the shared
`cururu-core` domain crate plus `cururu-engine` review contracts. Both Cururu
dependencies are pinned to the same Git revision so app updates adopt engine
changes explicitly; no crate release is required.

## Current slice

The current desktop slice includes local review with guarded suggestions,
GitHub PR context with explicitly confirmed inline-thread replies, and a
dual-provider second-opinion workflow. The review workspace includes:

- a repository file rail;
- a virtualized diff with 10,000 rows;
- selectable source lines and a finding detail pane;
- unified and side-by-side layouts with synchronized virtualized scrolling;
- separately approved local test/analyzer commands for an opened checkout.

Without an argument, the app renders a 10,000-row synthetic unified diff parsed
through the pinned `cururu-core` parser. The sample does not access a
repository or evaluator; GitHub is contacted only if you explicitly choose
**Connect GitHub**.

Local Git access uses the embedded `gix` library rather than a Git subprocess.
Open a worktree with the **Open repository…** folder picker, or from the command
line with `cargo run -- /path/to/repository`; the workspace lists staged,
unstaged, and untracked text changes and renders unified diffs through the pinned
`cururu-core` parser. Repository access is read-only. Text input is capped at
2 MiB per side/file and 32 MiB aggregate; rendered diffs are capped at 45,000
bytes per file and 180,000 bytes total. Binary files, symlinks, and non-UTF-8
paths are not rendered.

The app records the most recently opened repository's path, branch, and HEAD
object ID in a local SQLite database. **Reopen last session** explicitly resumes
it; the app does not auto-open repositories. SQLite stores no credentials or
source snapshots. GitHub tokens, when connected, are stored only in the
operating-system credential store. See [the architecture decisions](docs/decisions.md).

In an opened repository, select a line on the new side of the diff to write and
save a local comment draft. Drafts are anchored to the path, line, and observed
HEAD; they are never published. Drafts from a different HEAD are marked stale
and kept at their original anchor rather than silently moved or discarded.
Click a saved draft to edit it, or explicitly delete it; drafts do not expire
automatically. Multiple app instances are supported, and conflicting edits are
rejected rather than silently overwriting newer text.

Local review is an explicit, optional action for an opened worktree. Configure an
HTTPS OpenAI-compatible endpoint and model; the API key is stored in a dedicated
OS-keyring entry, separate from evaluator credentials. Saving settings and
authorizing a checkout send no code. Consent is stored per canonical checkout
path and provider/prompt fingerprint. Before the send action, the workspace shows
the destination, changed-file count, and limits. The base request sends changed-file
paths and bounded unified diffs (up to 100 files, 45,000 bytes per file, and
180,000 bytes total). Context/presentation data and normalized static evidence
require their separate explicit inclusion choices described above. The app rechecks consent and the exact diff before sending
and discards the result if the checkout changes during review. Findings are
informational and are never applied automatically. A structured single-line
suggestion can be previewed as a local diff; only a separate confirmation applies
it after rechecking the reviewed diff and exact file contents. Applying a
suggestion does not run commands, stage, commit, or push.

Local review reads a regular `.cururu.toml` at the checkout root (up to 64 KiB)
using the shared engine schema. Supported restriction fields are:
`review.max_diff_bytes`, `chunk_bytes`, `ignore`, and `policy.minimum_confidence`,
`max_findings`, `allowed_severities`, `suggested_changes`. Explicit profiles,
presentation, synthesis, informational quality status, incremental reuse and local
analysis artifacts are also supported under the consent boundaries above. Limits cannot exceed
the desktop ceilings. Valid policy-suppressed findings remain visible with a
reason. Provider settings in the repository are ignored: destination/model/key
and consent remain local. The config file's patch is not sent for review.
Configuration changes invalidate an in-flight review even when Git ignores the
file. Unsupported or invalid configuration blocks AI review; manual diff/drafts
remain available subject to the worktree reader's existing file limits and
non-symlink-file support. Unsupported configurations (including automatic extra
recommendation requests, remote check runs and unbound SARIF) block AI sends
rather than being silently ignored. `[summary]` is not applied; `[evaluator]` is local-only.

`policy.focus` and paths/globs under `context.conventions`, `specifications`,
`skills` and `additional` are optional evidence. Parsing only lists declarations:
**Discover declared paths (names only)** expands glob matches from the Git index
without opening declared contents. Inspect the resolved list, then **Load these
exact paths locally for preview** explicitly reads them. Finally,
**Include this previewed context** approves inclusion in the next review.
Alternatively choose **Review diff only** to explicitly exclude context/focus.
Nothing is sent by loading/previewing. These choices stay in memory and are
bound to the checkout and diff/configuration identity. Provider consent remains
separate. Context is JSON user data, never system instructions; skills are not
executed. Paths are descriptor-relative with no symlinks; credential locations,
hardlinks, special files, non-text and over-budget evidence are rejected. Limits
are 32 files, 16,000 bytes/file and 64,000 bytes total, lowered by
`context.max_bytes`. Evidence is repeated per diff chunk.
Only tracked regular, unconflicted Git index paths can match globs; exact paths
may explicitly name untracked files. Overlaps are deduplicated in declaration
order. Missing matches, sensitive/non-file paths and more than 32 resolved files
fail rather than being silently omitted. Changed membership invalidates the
manifest before any newly matched contents are opened.
`[context.auto]` reuses the CLI's shared defaults/matching rules to propose
changed files' **HEAD versions**, not their worktree contents. It still requires
Discover → Load → Include or diff-only; enabling it never starts I/O. The names
preview shows the full revision and blob identity. Added/unborn paths without a
HEAD version have no base candidate. Explicit declarations win duplicate paths.
Auto count/aggregate budgets include declared evidence. Desktop ceilings remain
32 files / 64,000 total bytes / 16,000 per file, lowered by configured limits;
unlike CLI best-effort loading, oversized/non-text/excess candidates fail instead
of silently truncating or skipping. Blob size is checked before content loading.
Context is re-read before/after sending and before previewing/applying a resulting
suggestion; a changed document invalidates approval, including ignored files.

Local tests and analyzers are a separate, manually entered workflow. Cururu
shows the resolved executable, literal arguments, checkout, working directory,
HEAD/diff identity, and isolation status, then requires a fresh confirmation for
each run. It launches directly without shell expansion, does not load repository
environment files or pass Cururu provider credentials, captures at most 32 KiB
per output stream, stops a run after 10 minutes, and never persists command
output. On Linux, a verified Bubblewrap sandbox makes the checkout read-only,
disables networking, and hides the user's home and credential stores. On macOS,
Seatbelt uses a deny-by-default profile: the checkout is read-only, networking
is disabled, writes are limited to a private temporary directory plus required
system device nodes, and the home directory is hidden except for the
checkout and allowlisted Rust toolchain/cache paths. Apple's system
`sandbox-exec` command is marked deprecated; if it or the
required profile is unavailable, the app offers only the explicit
unsandboxed-confirmation path. Otherwise, running without isolation requires a
separate confirmation and may access or modify anything permitted to the OS
user. Remote PR text never supplies or authorizes a command.

GitHub sign-in uses the Device Flow. Set the public GitHub App client ID in
`CURURU_GITHUB_APP_CLIENT_ID` after configuring an app with Device Flow enabled.
GitHub authentication and read-only pull-request inspection are available;
individual thread replies use the separate explicit write flow below.
**Disconnect GitHub** removes local credentials but does not revoke the grant at
GitHub.

Alternatively, explicitly choose **Use existing gh login (memory only)**. This
runs only `gh auth token --hostname github.com` from a supported installed CLI
location (system, Homebrew, or `~/.local/bin`), not from repository PATH entries.
The command has a five-second deadline and a 16 KiB output bound. Environment
tokens/provider keys are not inherited. It does not run login/logout, contact an
evaluator, or persist the returned token in Cururu. This fallback inherits the
existing gh credential's permissions, which may be broader than the GitHub App;
its expiry/revocation remain managed by gh/GitHub. Disconnect only clears app
memory. If rejected/expired, disconnect and explicitly select the fallback again.

With a valid GitHub session, enter a PR as `owner/repository#number` and choose
**Open GitHub PR** to load its metadata, changed-file patches, review
submissions, inline comment threads, and general conversation comments. Inline
comments whose current line is absent but original line is known are marked as
outdated locations; this does not mean the concern was resolved. PR loading is
read-only; choosing an inline comment focuses its file and diff
line when that anchor is present in the loaded patch, and loads a bounded source
excerpt at the comment's original commit. Missing or oversized patches fail
closed instead of showing an incomplete diff. Remote source is fetched only on
selection and kept in memory. Loading a remote PR does not send code to an
evaluator or publish comments. For a selected inline thread, you can compose an
individual reply. The app previews the exact PR, head, target comment, and body;
only **Confirm and publish this reply** sends it. Before sending, the app
rechecks that the PR is open, its head is unchanged, and the target comment
still exists. It does not automatically retry an ambiguous request; refresh the
PR before deciding whether to retry.

In **Review changes**, select a new-side line to queue a new inline comment.
Provide a review summary, then preview one comment or all queued comments for
the exact PR/base/head. Only **Confirm and publish this review** sends a single
GitHub `COMMENT` review with those comments and summary. The app rechecks that
the PR is open, base/head are unchanged, anchors remain in the diff, and identical
comments do not already exist. Requests are never retried automatically. Failed
or ambiguous writes keep drafts and require a PR refresh before another attempt.
Queued comments survive mode/layout changes. **Save queued comments and summary
locally** explicitly persists them in SQLite; saved drafts are restored on app
startup without sending anything. Later edits, removals, or publication require
another save to update that local snapshot. Concurrent saves from another app
instance are rejected instead of overwriting its drafts. Tokens and source
snapshots are never part of this record. Stale/other-PR drafts remain visible and
cannot be sent against a different PR/head. General conversation comments and approval or
request-changes votes are not implemented yet.

“Understand a review” can optionally run a second opinion through both a
user-configured OpenAI-compatible chat-completions endpoint and Jev. Configure
both models and enter both API keys in that mode; keys are stored only in the
OS keyring, while non-secret settings stay in local SQLite. The app requires
explicit local authorization for the exact `owner/repository` and provider
configuration, then a separate **Send this bounded thread** action. That
action sends the selected inline thread, a small source excerpt at the comment
commit, and at most 2,500 diff characters to both providers. Calls run
concurrently with a 2-minute deadline; no partial result is shown if either
provider fails. Results are informational only: they never filter findings,
modify code, or publish the draft reply. No evaluator is contacted during PR
loading or comment selection.

## Run

Requires Rust 1.99.0 or newer.

```sh
cargo run
```

On Linux, GPUI's X11/Wayland system dependencies must be installed. CI builds
Linux x86_64 and ARM64 AppImages, plus macOS Apple Silicon `.app`/`.dmg`
packages. Packaging is validation-only: CI does not sign, upload, or publish
these files. Native CI validated all three packaging targets in run
`38737417983`, including the macOS Seatbelt tests; runtime and visual validation
remain separate proof items.

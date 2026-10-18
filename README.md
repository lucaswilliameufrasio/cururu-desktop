# Cururu Desktop

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
the destination, changed-file count, and limits. Only changed-file paths and
bounded unified diffs are sent (up to 100 files, 45,000 bytes per file, and
180,000 bytes total). The app rechecks consent and the exact diff before sending
and discards the result if the checkout changes during review. Findings are
informational and are never applied automatically. A structured single-line
suggestion can be previewed as a local diff; only a separate confirmation applies
it after rechecking the reviewed diff and exact file contents. Applying a
suggestion does not run commands, stage, commit, or push.

Local review reads a regular `.cururu.toml` at the checkout root (up to 64 KiB)
using the shared engine schema. Supported restriction fields are:
`review.max_diff_bytes`, `chunk_bytes`, `ignore`, and `policy.minimum_confidence`,
`max_findings`, `allowed_severities`, `suggested_changes`. Limits cannot exceed
the desktop ceilings. Valid policy-suppressed findings remain visible with a
reason. Provider settings in the repository are ignored: destination/model/key
and consent remain local. The config file's patch is not sent for review.
Configuration changes invalidate an in-flight review even when Git ignores the
file. Unsupported or invalid configuration blocks AI review; manual diff/drafts
remain available subject to the worktree reader's existing file limits and
non-symlink-file support. Shared-base/analyzer sections and unsupported review/policy
fields are not implemented in the desktop yet and block AI sends rather than
being silently ignored. `[summary]` is not applied; `[evaluator]` is local-only.

`policy.focus` and exact paths under `context.conventions`, `specifications`,
`skills` and `additional` are optional evidence. Parsing only lists declarations:
**Load declared context locally for preview** explicitly reads them, then
**Include this previewed context** approves inclusion in the next review.
Alternatively choose **Review diff only** to explicitly exclude context/focus.
Nothing is sent by loading/previewing. These choices stay in memory and are
bound to the checkout and diff/configuration identity. Provider consent remains
separate. Context is JSON user data, never system instructions; skills are not
executed. Paths are descriptor-relative with no symlinks; credential locations,
hardlinks, special files, non-text and over-budget evidence are rejected. Limits
are 32 files, 16,000 bytes/file and 64,000 bytes total, lowered by
`context.max_bytes`. Evidence is repeated per diff chunk. Globs and automatic
discovery are unsupported (only `context.auto.enabled = false` is accepted).
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

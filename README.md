# Cururu Desktop

Cururu Desktop is a native code-review client built on GPUI and the shared
`cururu-core` domain crate plus `cururu-engine` review contracts. Both Cururu
dependencies are pinned to the same Git revision so app updates adopt engine
changes explicitly; no crate release is required.

## Current slice

The current desktop slice includes local review, read-only GitHub PR context,
and a dual-provider second-opinion workflow. The review workspace includes:

- a repository file rail;
- a virtualized diff with 10,000 rows;
- selectable source lines and a finding detail pane.

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
informational: the workflow does not edit files, run commands, stage, commit, or
publish anything.

GitHub sign-in uses the Device Flow. Set the public GitHub App client ID in
`CURURU_GITHUB_APP_CLIENT_ID` after configuring an app with Device Flow enabled.
GitHub authentication and read-only pull-request inspection are available;
remote comment publication is not implemented. **Disconnect GitHub** removes
local credentials but does not revoke the grant at GitHub.

With a valid GitHub session, enter a PR as `owner/repository#number` and choose
**Open GitHub PR** to load its metadata, changed-file patches, review
submissions, inline comment threads, and general conversation comments. Inline
comments whose current line is absent but original line is known are marked as
outdated locations; this does not mean the concern was resolved. This is
read-only; choosing an inline comment focuses its file and diff line when that
anchor is present in the loaded patch, and loads a bounded source excerpt at
the comment's original commit. Missing or oversized patches fail closed instead
of showing an incomplete diff. Remote source is fetched only on selection and
kept in memory. Loading a remote PR does not send code to an evaluator or
publish comments.

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

```sh
cargo run
```

On Linux, GPUI's X11/Wayland system dependencies must be installed. macOS
packaging and hardware validation remain a separate proof item.

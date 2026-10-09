# Cururu Desktop

Cururu Desktop is a native code-review client built on GPUI and the shared
`cururu-core` library. The core dependency is pinned to a Git revision so app
updates adopt engine changes explicitly; no crate release is required.

## Current slice

The first vertical slice is a GPUI feasibility pass for the review workspace:

- a repository file rail;
- a virtualized diff with 10,000 rows;
- selectable source lines and a finding detail pane.

Without an argument, the app renders a 10,000-row synthetic unified diff parsed
through the pinned `cururu-core` parser. This sample does not access a
repository, SCM credentials, an evaluator, or external services.

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
it; the app does not auto-open repositories. No credentials, source snapshots,
or cloud data are stored. See [the architecture decisions](docs/decisions.md).

## Run

```sh
cargo run
```

On Linux, GPUI's X11/Wayland system dependencies must be installed. macOS
packaging and hardware validation remain a separate proof item.

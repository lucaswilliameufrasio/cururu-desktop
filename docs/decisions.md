# Desktop architecture decisions

## Local Git access

- **Status:** Accepted
- **Date:** 2026-10-08

The desktop client will access local repositories through the embedded Rust
`gix` (gitoxide) library. It will not shell out to the user's `git` executable.
This avoids a runtime dependency on a separately installed Git binary and keeps
repository access behind an app-owned adapter.

The first repository integration is read-only: opening a repository and
inspecting its working tree must not update the index, refs, configuration, or
worktree. UI state and review policy remain in the desktop app; provider-neutral
diff parsing and anchors remain in `cururu-core`. Remote operations, staging,
commits, and other Git mutations are out of scope unless separately decided.

The initial dependency should disable unused `gix` defaults and enable only the
features required by local repository discovery/status. Review content must
continue to pass through the existing core diff contracts before rendering.

# Cururu Desktop

Cururu Desktop is a native code-review client built on GPUI and the shared
`cururu-core` library. The core dependency is pinned to a Git revision so app
updates adopt engine changes explicitly; no crate release is required.

## Current slice

The first vertical slice is a GPUI feasibility pass for the review workspace:

- a repository file rail;
- a virtualized diff with 10,000 rows;
- selectable source lines and a finding detail pane.

The 10,000-row unified diff is generated in memory and parsed through the pinned
`cururu-core` parser. This slice does not access a repository, SCM credentials,
an evaluator, or external services.

## Run

```sh
cargo run
```

On Linux, GPUI's X11/Wayland system dependencies must be installed. macOS
packaging and hardware validation remain a separate proof item.

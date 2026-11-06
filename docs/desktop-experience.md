# Desktop experience

## Redesign scope

The default launch is a real empty state, not a populated synthetic review.
Manual review does not require AI configuration or GitHub authentication.
Demo data remains available to tests, not as the normal first-run workspace.

## Journeys and ownership

- **Start:** choose a local checkout or open a GitHub pull request. Loading and
  failures stay at the entry point; successful loading opens the review.
- **Review:** files, diff and the selected discussion are the primary content.
  Switching destinations preserves drafts and never grants permission or sends code.
- **GitHub:** connect, enter a device code, cancel, retry and disconnect. Errors
  are shown here. The optional existing-gh session is a secondary, explicit path.
- **AI settings:** configure review and second-opinion providers separately.
  Saving credentials is not consent to send repository content.

The host owns navigation; existing domain modules retain authorization,
revision guards, persistence and publication. No framework migration is part of
this slice. HTTP must be installed explicitly: GPUI defaults to a null client.

## Visual system

Quiet native developer workspace: charcoal canvas #111820, raised surface
#18222D, borders #2B3946, primary text #E6EDF3, secondary #A6B5C5, blue focus
#79B8FF. Green and red are reserved for diff/status semantics. System sans for
controls and headings, monospace for code. Compact navigation and 32px page
padding; bounded-width forms instead of window-wide inputs. Native component
buttons provide consistent focus, hover, disabled and selected states.

The retrieved design-system suggestion included a documentation landing page;
that topology was rejected as irrelevant to a native review tool.

## Acceptance

- First launch presents next actions without fake findings or AI forms.
- Opening real content enters the review; navigation back preserves session data.
- GitHub authentication visibly progresses or exposes a recoverable error.
- Production HTTP reaches a local protocol fixture without Tokio.
- Provider settings do not occupy review space and keys remain in the OS keyring.
- Required verification includes local full gate and inspected native screenshots;
  macOS OAuth and target-device interactions must not be claimed from Linux tests.

This document describes the redesign direction and acceptance, not proof that
every journey has been visually validated.

## Local verification — 2026-11-06

Working branch: `feature/desktop-workflow-redesign`. Final local gate passed:

- `cargo fmt --all -- --check`
- `cargo clippy --locked --workspace --all-targets -- -D warnings`
- `cargo test --locked --workspace`: 224 passed, zero failed or ignored, including
  real temporary SQLite/filesystem/process tests and local HTTP protocol fixtures.
- `cargo build --locked --workspace` and the same command with `--release`
- `APPIMAGE_EXTRACT_AND_RUN=1 NO_STRIP=1 cargo packager --release --formats appimage`
- `bash -n scripts/build-linux-arm64.sh`, the offline evaluator benchmark and its
  three Python tests, and `git diff --check`.

The existing proc-macro-error2 future-compatibility warning remains. Benchmark
fixtures are synthetic, no predictions were scored, and evaluator filtering is
still disabled.

Native GPUI 0.2.2 frames were captured and inspected under private KWin Wayland
virtual displays (1440 × 1000 and 1100 × 800), with isolated home/config/data and
`--ui-preview`. No personal keyring/database was restored. Confirmed interactions:

- Start → GitHub by click; Client ID typing; numeric App ID rejected visibly;
  Tab focus and Enter navigation. Evidence: `/tmp/opencode/cururu-interaction-o4unjrmr/`.
- A typed comment survives Review → provider settings → Review; Checks opens
  separately. Evidence: `/tmp/opencode/cururu-interaction-xwo3jby6/`.
- Unified/split switching and horizontal scroll buttons expose long lines and
  restore their starting position. Evidence: `/tmp/opencode/cururu-interaction-yq972vrn/`.
- The small display originally clipped the inspector and Open action. The final
  window fits at 1052 × 752 content pixels; compact columns, wrapped controls and
  a bounded header keep both accessible. Horizontal scroll, Open → Start and
  GitHub navigation were exercised again. Before/after evidence:
  `/tmp/opencode/cururu-interaction-nz3k35iv/01-review-small-viewport.png`,
  `07-final-compact-review.png`, `08-compact-scrolled.png`,
  `09-header-open-start.png` and `10-compact-github.png` in the same directory.

Window initialization waits at most one second for display discovery. GPUI's
Wayland implementation has no primary display, so it selects the first available
display when necessary. Initial dimensions are capped at 1440 × 900 with 48px
screen margin. Unit regressions cover these bounds, compact column budgets,
Unicode row measurement and horizontal offset clamping.

Artifacts are local ephemeral evidence, not published assets. Owned app/compositor
sessions are stopped after verification; no user's graphical session is controlled.

**Still unverified:** real account OAuth, macOS runtime/visual acceptance, clipboard
integration, physical wheel/trackpad scrolling, arbitrary interactive resizing,
and target GPU/DPI equivalence. KWin fake-input axis events did not establish
hardware scroll behavior; a private X11 attempt rendered black. Neither is claimed
as validated. HTTP fixtures prove request transport, form length, status propagation
and redirect refusal, not successful account authorization. This slice is not full
product/design sign-off, a gpui-kit migration or release readiness.

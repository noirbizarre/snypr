# AGENTS.md

Repository Purpose: Snypr (bin `snypr`) is a screenshot, annotation, and
live-drawing tool for Hyprland and other wlroots-based Wayland compositors.

Build/Test:
- Full build: `cargo build` (or `mise run build`)
- Format: `cargo fmt --all` (or `mise run fmt`)
- Lint: `cargo clippy --all-targets --all-features -- -Dclippy::all` (or `mise run lint`)
- Test all: `cargo nextest run --all-features` (or `mise run test`)
- Test the toolkit-free build: `mise run test:core` (`--no-default-features --features notify`)
- Single test: `cargo nextest run --test <file> -- <name::path>` or fallback `cargo test <name>`
- Coverage: `mise run cover` (GTK build) and `mise run cover:core` (toolkit-free build)
- Spellcheck: `typos` (or `mise run spell`); `git-cliff` also runs it on the changelog at release time

Releases: orchestrated by gh-ship; `cliff.toml` and `.github/ship.yml` are the
contract, `git-cliff` derives both the version and `CHANGELOG.md` from the
commit messages. Never bump the version or tag by hand — the
`prepare-release` workflow owns `[package] version` in `Cargo.toml`. Check the
setup with `mise run dogfood` (`gh ship validate`); see CONTRIBUTING.md.

Packaging: PKGBUILD templates for the three AUR packages (`snypr-bin`, `snypr`,
`snypr-git`) live in `packaging/aur/`, pushed by `.github/workflows/aur.yml` on
`release: published`. `publish-release` ships a prebuilt Arch binary tarball
alongside the source tarball. Keep the `packaging/aur/*/PKGBUILD` install lists
in sync with the packager table in README.md.

Code Style:
- Edition 2024; use `anyhow::Result` for fallible public fns; prefer `?` and propagate errors; avoid `.unwrap()` outside tests unless guaranteed.
- Imports: group std / external / crate; avoid wildcard; keep ordering lexical; re-export only intentional items (see `lib.rs`).
- Types: use explicit `PathBuf`; share state through `Ctx = Arc<Context>` (see `src/context.rs`); prefer enums over strings for state (e.g. `PngCompression`, `SinkSpec`, `SelectionSpec`).
- Naming: snake_case for functions/vars, PascalCase for types/traits; modules named after their domain (`capture`, `annotate`, `output`, `ui`, `hypr`, `ipc`); constants UPPER_SNAKE; avoid abbreviations except well-known (`ctx`, `cfg`).
- Async: traits with `#[async_trait]`; pass cloned `Arc<Context>` rather than `&mut`; avoid blocking in async (use `tokio::task::spawn_blocking` for sync work).
- Error handling: never silence errors; use `anyhow!(...)` / `.context(...)` / `.with_context(...)` for context; return early on invalid state. Use `thiserror` for typed errors that callers branch on (e.g. `CaptureError`, `ProtocolError`); `ui::selector::Cancelled` is a hand-rolled unit error for the same purpose.
- CLI: derive `Parser`/`Subcommand`; keep help strings imperative; prefer explicit flags (`--per-output`, `--via-daemon`); document precedence in doc-comments when CLI/config/IPC fields overlap.
- Configuration: every field optional; types live in `src/config.rs`; the source of truth for default values is `impl Default`. Keep README/manpage examples in sync.
- Formatting enforced by `cargo fmt`; do not hand-align; trailing whitespace stripped by prek.
- Tests: use `rstest` for parametrization; assertions via `pretty_assertions` when readability matters; unit tests live beside code under `#[cfg(test)]`. Shared fixtures (notably `test_ctx`) live in `src/testing.rs`, which is `#[cfg(test)]`-only.
- Integration tests requiring a live Wayland compositor live in `tests/wayland.rs` behind the `integration-wayland` feature. They drive the real `zwlr_screencopy` path and skip themselves when the protocol is unavailable — CI's headless Weston implements `weston_screenshooter`, not the wlroots protocol, so they are inert there. Run them from a wlroots session with `SNYPR_REQUIRE_WAYLAND_CAPTURE=1` to turn the skip into a hard failure.
- GTK-backed tests: widget-level tests need a `GdkDisplay`. Start them with the `require_gtk!()` macro (`src/ui/mod.rs`), which skips the test when no compositor is reachable so `cargo test` still works headless-less. It expands to a bare `return`, so it only works in tests returning `()`. CI starts a headless Weston (`.github/actions/headless-compositor`) and sets `SNYPR_REQUIRE_GTK=1`, which turns "no display" into a hard failure so the skip can never hide a regression; the variable is read by *value*, so `SNYPR_REQUIRE_GTK=0` opts back out. To reproduce locally: `weston --backend=headless-backend.so --socket=snypr-ci --idle-time=0 &` then run with `WAYLAND_DISPLAY=snypr-ci`.
- The `ui` feature must stay optional in practice, not just on paper: `src/daemon.rs` once referenced `crate::ui` unconditionally and broke `--no-default-features` silently. The `test-core` CI job and `mise run lint:core` exist to keep that from recurring. Anything that imports no toolkit type belongs outside `src/ui/` (see `src/save.rs`).

Coverage: CI uploads twice, under two Codecov flags — `core` (`--no-default-features --features notify`, no compositor) and `gtk` (`--all-features` under headless Weston). `codecov.yml` also slices both by module via `component_management`. When adding a module, add it to a component so it does not fall off the per-area report.
- Git hooks: commit messages follow Commitizen (conventional commits); prek runs cargo-fmt, cargo-clippy, actionlint, Taplo TOML formatting, commitizen,
  and the usual whitespace/YAML/TOML hygiene hooks.

General: Do not add new dependencies lightly; prefer existing patterns
(notifications via `notify-rust` wrapped in `src/notify.rs`, GTK styling via
`ui::style`, window-manager IPC via the in-tree `src/wm/` backends — Hyprland's
own socket, Sway's i3ipc socket — rather than upstream crates). Update docs
(README, `docs/man/snypr.1`) when user-facing behavior changes.

Translations: user-facing UI strings (toolbar/selector/tray tooltips, desktop
notifications, errors surfaced via `eprintln!` / `notify_error`) go through
Fluent via the `fl!` macro re-exported from `src/i18n.rs`. Catalogs live at
`i18n/<lang>/snypr.ftl` and are embedded into the binary. English (`en`) is
the fallback and source of truth; add a new language by dropping a new file in
`i18n/<code>/` and translating every key. The active locale is resolved at
startup with precedence `--lang` flag > `language` config field >
`LC_ALL`/`LC_MESSAGES`/`LANG` > `en`. Developer-oriented strings (tracing logs,
low-level I/O `anyhow::Context`, the `doctor` Markdown report, clap `--help`)
stay English on purpose.

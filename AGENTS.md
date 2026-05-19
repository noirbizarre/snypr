# AGENTS.md

Repository Purpose: HyprSnap (bin `hyprsnap`) is a screenshot, annotation, and
live-drawing tool for Hyprland and other wlroots-based Wayland compositors.

Build/Test:
- Full build: `cargo build` (or `mise run build`)
- Format: `cargo fmt --all` (or `mise run fmt`)
- Lint: `cargo clippy --all-targets --all-features -- -Dclippy::all` (or `mise run lint`)
- Test all: `cargo nextest run` (or `mise run test`)
- Single test: `cargo nextest run --test <file> -- <name::path>` or fallback `cargo test <name>`
- Coverage: `cargo llvm-cov nextest` (or `mise run cover`)

Code Style:
- Edition 2024; use `anyhow::Result` for fallible public fns; prefer `?` and propagate errors; avoid `.unwrap()` outside tests unless guaranteed.
- Imports: group std / external / crate; avoid wildcard; keep ordering lexical; re-export only intentional items (see `lib.rs`).
- Types: use explicit `PathBuf`; share state through `Ctx = Arc<Context>` (see `src/context.rs`); prefer enums over strings for state (e.g. `PngCompression`, `SinkSpec`, `SelectionSpec`).
- Naming: snake_case for functions/vars, PascalCase for types/traits; modules named after their domain (`capture`, `annotate`, `output`, `ui`, `hypr`, `ipc`); constants UPPER_SNAKE; avoid abbreviations except well-known (`ctx`, `cfg`).
- Async: traits with `#[async_trait]`; pass cloned `Arc<Context>` rather than `&mut`; avoid blocking in async (use `tokio::task::spawn_blocking` for sync work).
- Error handling: never silence errors; use `anyhow!(...)` / `.context(...)` / `.with_context(...)` for context; return early on invalid state. Use `thiserror` for typed errors that callers branch on (e.g. `CaptureError`, `ProtocolError`, `ui::selector::Cancelled`).
- CLI: derive `Parser`/`Subcommand`; keep help strings imperative; prefer explicit flags (`--per-output`, `--via-daemon`); document precedence in doc-comments when CLI/config/IPC fields overlap.
- Configuration: every field optional; types live in `src/config.rs`; the source of truth for default values is `impl Default`. Keep README/manpage examples in sync.
- Formatting enforced by `cargo fmt`; do not hand-align; trailing whitespace stripped by prek.
- Tests: use `rstest` for parametrization; assertions via `pretty_assertions` when readability matters; unit tests live beside code under `#[cfg(test)]`. Integration tests requiring a live Wayland compositor go behind the `integration-wayland` feature.
- Git hooks: commit messages follow Commitizen (conventional commits); prek runs cargo-fmt, cargo-clippy, and Taplo TOML formatting.

General: Do not add new dependencies lightly; prefer existing patterns
(notifications via `notify-rust` wrapped in `src/notify.rs`, GTK styling via
`ui::style`, Hyprland IPC via the in-tree `src/hypr.rs` rather than the upstream
`hyprland` crate). Update docs (README, `docs/man/hyprsnap.1`) when user-facing
behavior changes.

Translations: user-facing UI strings (toolbar/selector/tray tooltips, desktop
notifications, errors surfaced via `eprintln!` / `notify_error`) go through
Fluent via the `fl!` macro re-exported from `src/i18n.rs`. Catalogs live at
`i18n/<lang>/hyprsnap.ftl` and are embedded into the binary. English (`en`) is
the fallback and source of truth; add a new language by dropping a new file in
`i18n/<code>/` and translating every key. The active locale is resolved at
startup with precedence `--lang` flag > `language` config field >
`LC_ALL`/`LC_MESSAGES`/`LANG` > `en`. Developer-oriented strings (tracing logs,
low-level I/O `anyhow::Context`, the `doctor` Markdown report, clap `--help`)
stay English on purpose.

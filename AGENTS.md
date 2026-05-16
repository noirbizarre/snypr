# AGENTS.md

Repository Purpose: HyprSnap (bin `hyprsnap`) is a snapshot/screenshot/annotation tools for Hyprland.

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
- Types: use explicit `PathBuf`, `Arc<Context>`; alias errors with `Result<T, anyhow::Error>` unless using boxed dynamic (`Expect`); prefer enums over strings for state.
- Naming: snake_case for functions/vars, PascalCase for types/traits; modules concise (`fs`, `git`); constants UPPER_SNAKE; avoid abbreviations except well-known (`ctx`).
- Async: traits with `#[async_trait]`; pass cloned `Arc` rather than &mut; avoid blocking in async (wrap with `spawn_blocking`).
- Error handling: never silence errors; use context via `anyhow!(...)` or `.with_context(...)`; return early on invalid state.
- CLI: derive `Parser`/`Subcommand`; keep help strings imperative; prefer explicit flags (`--dry-run`).
- Formatting enforced by `cargo fmt`; do not hand-align; trailing spaces removed (prek).
- Tests: use `rstest` for parametrization; assertions via `pretty_assertions` when readability matters; unit tests live beside code under `#[cfg(test)]`.
- Git hooks: commit messages follow Commitizen (conventional commits); prek runs fmt, clippy, Taplo.

General: Do not add new dependencies lightly; prefer existing patterns (progress bars via `indicatif`, styles via `ui::style`). Update docs only if user-facing behavior changes.

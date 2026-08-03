# AGENTS.md

## Tasks

This repo uses [mise](https://mise.jdx.dev) for task orchestration. All
build, test, and lint commands are mise tasks defined in `mise.toml`.
Run them via `mise run <task>`:

- `mise run check` — compile check (`cargo check`)
- `mise run test` — run tests (`cargo test`)
- `mise run fmt` — check formatting (`cargo fmt --check`)
- `mise run clippy` — lint with pedantic (`cargo clippy -- --deny clippy::pedantic`)

Before first run: `mise install` to set up the Rust toolchain.

## Code Style

- Clippy pedantic is enforced in CI. Run `mise run clippy` locally before
  committing — CI will fail on any `clippy::pedantic` warning.
- Run `cargo fmt` to fix formatting before `mise run fmt`.
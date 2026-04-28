# Repository Guidelines

## Project Overview

`org-cli` is a Rust CLI for querying and editing org-mode files. It provides
machine-friendly output formats (NUL-separated, NDJSON) alongside human-readable
output, enabling integration with shell pipelines and programmatic tools.

## Project Structure & Module Organization

```
src/
├── main.rs            # Entry point, delegates to cli::run()
├── lib.rs             # Library surface for embedding
├── cli/
│   ├── mod.rs         # CLI dispatch and run()
│   ├── def.rs         # clap Parser definitions (Args, Commands)
│   └── action/        # Subcommand implementations
│       └── mod.rs
└── org/               # Org-mode parsing and manipulation
    └── mod.rs
```

- `src/cli/` hosts command definitions and argument parsing via `clap`.
- `src/org/` centralizes org-mode parsing logic.
- `tests/` contains integration tests that spawn the actual binary.
- `tests/fixtures/` holds sample org files for testing.
- `doc/plan/` contains development plans.
- `doc/agent/` stores agent reports (prefix with `YYYYMMDD-`).

## Build, Test, and Development Commands

```bash
cargo check                              # Fast type check
cargo build                              # Compile debug build
cargo build --release                    # Optimized binary
cargo run -- --help                      # Inspect CLI options
cargo fmt                                # Format code (mandatory)
cargo clippy --all-targets --all-features  # Lint (warnings as errors)
cargo test                               # Run all tests
cargo test -- --nocapture                # Debug failing tests
cargo doc --no-deps                      # Build documentation
```

## Development Workflow (TDD)

Follow this sequence for all changes:

1. **Write test first** - Define expected behavior before implementation
2. **Implement** - Write minimal code to pass tests
3. **Corner cases** - Add edge case tests, iterate
4. **Document** - Add `///` doc comments for public APIs
5. **Format** - `cargo fmt`
6. **Lint** - `cargo clippy --all-targets --all-features`
7. **Review/Refactor** - Self-review pass (see checklist below)

## Review/Refactor Checklist

After passing all tests and lints, review the produced code:

| Category | Questions |
|----------|-----------|
| Duplication | Unnecessary repetition? Extract shared logic? |
| Clarity | Variable/function names consistent and descriptive? |
| Complexity | Can functions be simplified? Single responsibility? |
| Layout | Code logically organized? Related items grouped? |
| Security | Input validated? No panics in library code? |
| Resilience | Graceful error handling? Meaningful error messages? |
| Elegance | Idiomatic Rust? Appropriate abstractions? |
| Performance | Unnecessary allocations? Efficient iterator use? |

## Coding Style & Naming Conventions

- Use default Rust 4-space indentation.
- `snake_case` for functions, variables, modules, file names.
- `PascalCase` for types and traits.
- `SCREAMING_SNAKE_CASE` for constants.
- Prefer `///` doc comments for public APIs.
- Use `//` for implementation notes.
- Let `rustfmt` and `clippy` enforce style; do not hand-tune formatting.

## Testing Guidelines

- **Unit tests**: Co-locate in `mod tests` blocks gated by `#[cfg(test)]`.
- **Integration tests**: Place in `tests/` directory, spawn actual binary.
- **Fixtures**: Use `tests/fixtures/` for sample org files.
- **Determinism**: Use explicit data, avoid system-dependent values.
- Name test functions descriptively: `test_parse_heading_with_tags`.

## Output Formats

All commands must support three output modes:

| Format | Flag | Purpose |
|--------|------|---------|
| Human | (default) | Readable terminal output |
| NUL | `-0` | Fixed-field records for bash piping |
| NDJSON | `-j`/`--json` | One JSON object per line |

## Commit Message Format

Format: `{type}: [{scope}:] [{module}] {description} [!minor]`

- Types: `fix:`, `chg:`, `new:`
- Scopes (optional, no user-facing impact): `dev:`, `doc:`, `test:`, `pkg:`
- Use double-backticks around technical terms
- No `Co-Authored-By` line

## Pull Request Guidelines

- Concise summary of changes
- Testing checklist
- Link related issues
- All mandatory checks must pass:
  - `cargo fmt` (no changes)
  - `cargo clippy` (no warnings)
  - `cargo test` (all pass)

## Agent Planning

- Write development plans to `doc/plan/`.
- Write agent reports to `doc/agent/` with `YYYYMMDD-` prefix.
- Example: `20260113-report-library-selection.md`.

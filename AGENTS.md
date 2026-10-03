# Storycast — code style

Rules for new code and for refactors. `make test` (`cargo test --workspace` +
`cargo clippy --all-targets -- -D warnings`) must pass on every commit.

## File size

- Split a file when it passes **~800 lines** (or a single fn/impl passes ~300).
- Split along the `// --- section ---` markers, or by cohesive domain.
- The pattern is `foo.rs` (module doc, shared types, `mod` decls, re-exports)
  plus `foo/<section>.rs` children. This works for non-root modules; children
  of a crate's `main.rs` root live as plain `src/<name>.rs` siblings.
- An inherent `impl` may continue across files (`impl Inner { ... }` in each
  child) when the impl is the natural seam.

## Comments

- Comments are short and only when the code cannot explain itself.
- `//!` module head: at most ~3 lines, what the module owns.
- `///` doc: one short line on public items where the signature is not enough;
  none on private items unless a non-obvious constraint demands it.
- `//` inline: constraints and invariants only — the *why*, never the *what*.
  No design history, no incident reports, no measurement prose: git history is
  the archive. (Policy since the 1bae6fa trim; the pre-trim essays live in the
  history of commit 95a9b44 and earlier.)
- Clap `///` help is user-facing and stays. Prompt strings, error strings and
  test fn names are content, not comments.

## Tests

- Inline test modules move to a sibling test file (`foo/tests.rs`, wired with
  `#[cfg(test)] mod tests;`, opening with `use super::*;`). A test file over
  the cap splits into `foo/tests/<area>.rs` children.
- Shared fixtures live in the tests root so every child sees them.
- Manual live-backend checks stay in `bm-core/tests/` (run by hand, not CI).

## Imports and visibility

- Children start with `use super::*;` — the parent's own imports transfer.
  `super::` for parent/siblings, `crate::` for other modules, full
  `bm_core::...` paths cross-crate.
- Lib crates: private `mod x;` + curated `pub use` at the module root, so no
  public path ever moves. Binary crates: `pub(crate) mod x;`.
- Items moved into a sibling that the old layout reached privately widen to
  `pub(crate)`, exactly as far as the new boundary requires.

## Commits

- Subject: `area: lowercase sentence` (or `refactor:`, `style:`, `docs:`).
- Body: what moved/changed and why it is safe; end with the test count and
  `clippy -D warnings clean`.
- No co-author or attribution trailers.

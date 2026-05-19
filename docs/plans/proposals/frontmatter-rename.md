# Proposal: `frontmatter-rename`

**Task:** Phase 1 / `frontmatter-rename` from `docs/plans/2026-05-18-typed-task-invocation.md`
**Author:** `impl-frontmatter-rename`
**Status:** awaiting approval

Extend `src/bin/rnme/frontmatter.rs` to parse the optional `[rnme.rename]` section and surface the substituted name (raw, pre-normalization) on `Frontmatter`. No behavior wiring — `apply-rename` is the consumer.

## 1. Field shape

Proposal: keep the field a plain `Option<String>` for now.

```rust
pub struct Frontmatter {
    pub dependencies: Vec<(String, String)>,
    pub rename: Option<String>, // raw substituted name (pre-normalization)
}
```

Rationale:
- The design doc (§6) defines exactly one key under `[rnme.rename]`: `name`. Nothing else is contemplated.
- The plan's task summary specifies `pub rename: Option<String>` literally.
- Wrapping in a struct (`Option<Rename { name: String }>`) would be future-proofing the design doc didn't ask for and would add a public type with one field.
- If a sibling key ever shows up later, the change is mechanical (replace `Option<String>` with `Option<Rename>` everywhere — currently zero consumers).

OPEN — none. Will adopt `Option<String>` unless `lead` directs otherwise.

## 2. Parse strategy

The existing parser is hand-rolled. It walks lines, recognizes `//!`-prefixed doc comments, toggles into a `[dependencies]` section when it sees the section marker, and stops at the first non-`//!` line. No TOML library is involved.

Plan: extend the same hand-rolled walker. The section marker `[rnme.rename]` and its single `name = "..."` line are simple enough that pulling in a TOML parser is not justified — and is inconsistent with the rest of the file.

Concrete additions:

1. Introduce a small section-state enum so the loop is no longer a single `in_deps_section: bool`. Two sections need to coexist with each other and with a "no section" idle state. Sketch:

   ```rust
   enum Section { None, Dependencies, RnmeRename }
   ```

2. Recognize `[rnme.rename]` exactly the same way `[dependencies]` is recognized today: a `//!`-prefixed line whose trimmed content is the literal section header.

3. Inside the `RnmeRename` section, attempt to read a `name = "value"` line. The parsing reuses the same shape as `parse_dependency_line` but extracts only the value if the key is `name` and the value is a double-quoted string literal. Strip the surrounding quotes — the field stores the *raw substituted name* (e.g., `foo_bar_dashed`), not the quoted token (`"foo_bar_dashed"`).

4. Sections terminate the same way `[dependencies]` does today: by the first non-`//!` line, or by another `//!`-prefixed section header. The two sections may appear in either order; both, neither, or only one is valid.

5. Multiple `name = ...` lines inside a single `[rnme.rename]` section: take the last one and discard prior values silently. This matches how a naive TOML parser would behave and avoids inventing a "duplicate key" error case that the design doc didn't ask for.

6. Multiple `[rnme.rename]` sections in one file: same — last one wins. Same rationale.

## 3. Malformed input handling

Per the plan (line 247) and design doc (§6 — "the build fails with an error that names both colliding paths" is the *collision* error, not the malformed-rename error; the design doc is silent on malformed-rename specifically).

The plan's acceptance criterion is "Unit tests cover present, absent, and malformed cases" — without prescribing warn vs. error. The plan body says "warn or error per design call".

Proposed policy: **warn-and-ignore at parse time; error at the workspace-generation site (`apply-rename`).**

Rationale:
- `parse_frontmatter` currently returns `Frontmatter` infallibly. Making it return `Result<Frontmatter, Error>` ripples into every existing caller for a corner case that isn't load-bearing yet.
- The plan is explicit that this task "just exposes the field — no behavior wiring". A hard error at parse time *is* behavior wiring.
- `apply-rename` is the right place to fail: it has the file path in hand for a good error message ("error in `.../foo/RUNME.rs`: `[rnme.rename]` section is missing `name = \"…\"`"), and it can include the exact file context that the parser doesn't have.

Concretely, "malformed" cases this task will *recognize* (i.e., not parse as a valid rename) and surface as `rename: None`:

| Case | Parser behavior |
|---|---|
| Empty section (`[rnme.rename]` with no `name = ...` line) | `rename: None` |
| `name` with empty string value (`name = ""`) | `rename: None` (treat empty as absent) |
| `name` with non-string value (`name = 42`, `name = [1, 2]`) | `rename: None` |
| Missing quotes (`name = foo_bar`) | `rename: None` |
| Unknown key inside section (`title = "foo"`) | `rename: None` (the section yielded no `name`) |
| `name = "..."` with control chars / non-ident characters | `rename: Some("…")` — *parser stores raw*; downstream normalization handles validation |

The parser is forgiving and unopinionated. Downstream code can promote any of these to an error message with file context if it chooses. For the present task, the only observable effect is "the field is or isn't populated". This keeps the parser simple, keeps the public API infallible, and defers the policy decision to the consumer.

OPEN: If `lead` prefers the parser to return a `Result` and fail hard at parse time, that's a single signature change and I'll re-do the malformed cases as parse errors.

## 4. Storage semantics

The value is stored **raw**, exactly as it appears between the quotes in the source. No normalization, no validation against ident rules, no case folding, no dash/underscore substitution.

Rationale (from design doc §6):
> "The replacement string is substituted for the directory name **before normalization**. The same normalization pass then runs on the new name."

This is explicit: substitution comes first, normalization comes later in the pipeline. Storing a normalized value here would either (a) duplicate normalization in two places (here and in `apply-rename`), or (b) silently lose the original substituted spelling, which a future error message ("the substituted name `Hello World` normalized to `hello_world`") might want to reproduce verbatim.

Examples (parsed value):

| Source | `frontmatter.rename` |
|---|---|
| `//! name = "foo_bar_dashed"` | `Some("foo_bar_dashed")` |
| `//! name = "Hello World"` | `Some("Hello World")` |
| `//! name = "foo-bar"` | `Some("foo-bar")` |

The reader's expectation in `apply-rename` is: take the raw string, substitute it for the directory's basename in the path-to-ident pipeline, and let `crate_name.rs` normalize.

## 5. Test plan

Unit tests, added to the existing `#[cfg(test)] mod tests` block, mirroring the style and naming of the existing dependency tests:

| Test name | Scenario | Asserts |
|---|---|---|
| `test_rename_absent` | source with no `[rnme.rename]` section | `fm.rename == None` |
| `test_rename_present` | bare `[rnme.rename]\n//! name = "foo_bar_dashed"` | `fm.rename == Some("foo_bar_dashed".into())` |
| `test_rename_alongside_dependencies` | both `[dependencies]` and `[rnme.rename]` in same file, in either order | both fields populated; one test per order (`deps` first, `rename` first) |
| `test_rename_raw_not_normalized` | `name = "Hello World"` | `fm.rename == Some("Hello World".into())` — confirms no normalization at parse time |
| `test_rename_raw_preserves_dashes` | `name = "foo-bar"` | `fm.rename == Some("foo-bar".into())` |
| `test_rename_empty_section` | `[rnme.rename]` followed by no `name = ...` line | `fm.rename == None` |
| `test_rename_empty_string` | `name = ""` | `fm.rename == None` (empty treated as absent) |
| `test_rename_missing_quotes` | `name = foo_bar` (bareword) | `fm.rename == None` |
| `test_rename_non_string_value` | `name = 42` | `fm.rename == None` |
| `test_rename_unknown_key` | `[rnme.rename]\n//! title = "foo"` | `fm.rename == None` |
| `test_rename_last_wins` | two `name = ...` lines in the section | `fm.rename == Some(<second value>)` |
| `test_rename_stops_at_non_comment` | `[rnme.rename]` followed by a non-`//!` line, then `//! name = "x"` | `fm.rename == None` — confirms the existing termination rule applies |

All existing `test_*` tests for dependencies remain untouched and must continue to pass.

## 6. Decisions to confirm

1. `Option<String>` field, not a wrapper struct? (§1)
2. Hand-rolled extension to existing parser, no TOML library? (§2)
3. Parser is forgiving (returns `rename: None` on malformed input) — errors are deferred to `apply-rename`? (§3)
4. Test coverage as listed in §5?

Awaiting `lead` sign-off before implementing.

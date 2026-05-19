# Fixture — Proposal

**Task:** `fixture` (Phase 2, plan §426)
**Author:** `impl-fixture`
**Status:** awaiting review
**Scope:** Build an integration-test fixture that exercises every property the typed-task-invocation acceptance criteria depend on (in-file typed calls, cross-file typed calls, three arg forms, `[rnme.rename]`, dynamic-path agreement, type export, sibling collision).

This proposal commits to layout, RUNME.rs contents, test-driver shape, negative-case handling, and phasing so the fixture is in place before `val-typed-shim` / `val-subtasks` need it.

---

## 1. Layout

The fixture is a checked-in RUNME tree (not a cargo workspace member) plus a Rust integration test that drives the `rnme` CLI against it. This mirrors the existing `tests/cli_integration.rs` pattern (CLI subprocess + assertions over stdout/stderr/logs) and avoids gymnastics around making the fixture's generated workspace cohabit with rnme's own workspace.

```
testing/
  test-tasks/                          (existing — unchanged)
  fixtures/                            (new dir, not a cargo crate)
    typed_invocation/                  (positive-case fixture tree)
      RUNME.rs                         (root)
      child_a/
        RUNME.rs                       (in-file call demo lives here)
      services/
        RUNME.rs                       (intermediate-tier RUNME)
        api/
          RUNME.rs                     ([rnme.rename] name = "api_v2" — exercises substitution end-to-end)
      structural_only/                 (no RUNME.rs — intermediate dir)
        leaf/
          RUNME.rs
    typed_invocation_collision/        (negative-case: unresolved sibling collision)
      RUNME.rs
      foo-bar/
        RUNME.rs
      foo_bar/
        RUNME.rs
    typed_invocation_collision_resolved/  (positive-case: same pair, one renamed)
      RUNME.rs
      foo-bar/
        RUNME.rs                       ([rnme.rename] name = "foo_bar_dashed")
      foo_bar/
        RUNME.rs
tests/
  typed_invocation.rs                  (new integration test — drives all three fixtures via the rnme CLI)
```

Rationale for keeping the fixture out of the cargo workspace:

- Each RUNME.rs becomes a generated lib crate at `rnme` invocation time; the test driver doesn't need cargo to pre-resolve these as workspace members.
- This matches the existing `tests/cli_integration.rs` pattern (temp dirs with `write_fixture`), with the difference that *these* trees are checked in (so the structure is reviewable in PR diffs, and so we don't have to re-write them inside each test fn).
- No new `Cargo.toml` is required. No workspace `members` edit. The `testing/test-tasks` crate stays as-is.

The collision fixture lives in a separate sibling dir (`typed_invocation_collision/`) — not under `typed_invocation/` — because rnme's discovery walks down from the nearest RUNME.rs, and we want the negative case to be invocable without the positive-case build seeing it.

---

## 2. RUNME.rs contents skeleton

Brief contents per file. Exact code is generated when the fixture is wired in; this proposal commits to the shape.

### `testing/fixtures/typed_invocation/RUNME.rs` (root)

Three roles in this file:

- A **zero-args** task `root_noop`.
- A task `caller_in_file` that demonstrates **in-file typed call**: it calls `root_noop(ctx).await?` directly. Caller and callee in the same RUNME.rs (Acceptance §3 of the plan).
- A task `caller_cross_file` that demonstrates **cross-file typed call**: it calls `subtasks::services::api_v2::deploy(ctx, ApiDeployOpts { ... }).await?` (Acceptance §4 of the plan). Note the path uses the renamed identifier, not the on-disk dir name.
- A task `caller_dynamic` that demonstrates the **dynamic path**: it calls `ctx.run("services/api_v2:deploy", &[...]).await?` (Acceptance §7).
- A task `caller_uses_child_type` that **constructs an args struct exported by a descendant**: `let opts = subtasks::services::api_v2::ApiDeployOpts { ... };` (Acceptance §6).

### `testing/fixtures/typed_invocation/child_a/RUNME.rs`

A small leaf RUNME with a **simple-primitives** task `build(ctx, release: bool, verbose: bool)`. Its body just logs. Demonstrates Form-2 args.

### `testing/fixtures/typed_invocation/services/RUNME.rs`

Intermediate-tier RUNME (has its own RUNME.rs *and* descendants). Defines a single **zero-args** task `services_overview` whose body logs. The presence of this file exercises the "intermediate tier RUNME.rs does not break ancestor `subtasks::descendant::...` paths" property (Acceptance §6 of the plan), because the root still reaches `subtasks::services::api::deploy` *through* this intermediate.

### `testing/fixtures/typed_invocation/services/api/RUNME.rs`

Frontmatter:
```rust
//! [rnme.rename]
//! name = "api_v2"
```

The rename here is **structurally meaningful**: the on-disk directory is `api/` but the substituted-then-normalized identifier is `api_v2`, so the rename must flow through three call sites for anything that touches this RUNME.rs to work:

- Cargo crate name → `api_v2_crate` (or whatever crate-name normalization produces).
- Inventory group key → `api_v2`. The dynamic path `ctx.run("services/api_v2:deploy", &[])` resolves under this name.
- `subtasks` module path → ancestors call `subtasks::services::api_v2::deploy(...)`, not `subtasks::services::api::deploy(...)`.

If any of those three drift apart, at least one of the typed/dynamic-path tests below will fail. The no-op variant (rename string equal to the dir basename) would not catch drift.

Defines two tasks:

- A **parser-struct** task `deploy(ctx, opts: ApiDeployOpts)` where `ApiDeployOpts` is a `pub struct` defined in the same file with `#[derive(clap::Parser, Clone, Debug)]`. This is the Form-3 task. The body logs the opts and Ok(())'s.
- A **simple-primitives** task `health(ctx, port: u16)` — Form-2, used by an alternative caller if needed.

Exports `pub struct ApiDeployOpts` so the parent root RUNME.rs can construct it via `subtasks::services::api_v2::ApiDeployOpts { ... }` (Acceptance §6 of the plan).

### `testing/fixtures/typed_invocation/structural_only/leaf/RUNME.rs`

A leaf at the bottom of a chain whose intermediate dir (`structural_only/`) has **no RUNME.rs**. Tests that structural-only intermediate dirs surface in the parent's `subtasks::structural_only::leaf::...` (Acceptance §1 of the brief — "at least one structural-only intermediate dir").

Defines one zero-args task `leaf_task` logging its identity.

### `testing/fixtures/typed_invocation_collision/RUNME.rs`

Root for the negative fixture. Defines a single trivial task `noop`. No frontmatter.

### `testing/fixtures/typed_invocation_collision/foo-bar/RUNME.rs`

Trivial task `from_dashed`. No `[rnme.rename]`.

### `testing/fixtures/typed_invocation_collision/foo_bar/RUNME.rs`

Trivial task `from_undered`. No `[rnme.rename]`.

Both children normalize to the same Rust identifier (`foo_bar`) — when collision-detection lands, `rnme list` against this fixture must fail with the paste-ready error.

### `testing/fixtures/typed_invocation_collision_resolved/` (positive variant of the collision case)

Same directory pair as `typed_invocation_collision/`, with one sibling renamed to disambiguate. This is the fixture state plan §391 calls out: "Same fixture with one of them adding `[rnme.rename] name = "foo_bar_dashed"` builds cleanly."

`testing/fixtures/typed_invocation_collision_resolved/RUNME.rs` — trivial root, defines `noop`.

`testing/fixtures/typed_invocation_collision_resolved/foo-bar/RUNME.rs`:
```rust
//! [rnme.rename]
//! name = "foo_bar_dashed"
```
Defines a trivial task `from_dashed_resolved`.

`testing/fixtures/typed_invocation_collision_resolved/foo_bar/RUNME.rs` — no frontmatter. Defines a trivial task `from_undered_resolved`.

After resolution, the two children normalize to distinct identifiers (`foo_bar_dashed` and `foo_bar`), so the build must succeed. This fixture also doubles as a second exercise of the rename plumbing (independent of the `api_v2` rename in the main positive fixture).

---

## 3. Test driver shape

A single integration test file: `tests/typed_invocation.rs`. Pattern follows existing `tests/cli_integration.rs` — subprocess the `rnme` binary against a fixture dir, then assert against `stdout` / `stderr` / `exit_code`.

**Fixture isolation: copy-to-tempdir per `LazyLock<TempDir>` block**, matching `tests/cli_integration.rs:18-19`. Each of the three fixture trees (positive, collision, collision-resolved) is checked in under `testing/fixtures/` as the canonical *source*, but at test setup the source tree is copied into a `TempDir` (one `LazyLock<TempDir>` per fixture, shared across the tests in this file that target the same fixture). rnme runs against the temp copy; the workspace cache (`.rnme/` or wherever rnme stashes it) lands in the temp dir and is cleaned up at process exit.

This buys two properties the lead called out:
- Parallel `cargo test` runs do not race against a shared cache directory.
- Failed tests do not leave stale state in the working tree.

The one-time `cp -r` cost is amortized over all tests that share a `LazyLock<TempDir>`, so it's a single copy per fixture per test-binary run. A small helper `copy_fixture_to_tempdir(src: &Path) -> TempDir` lives at the top of `tests/typed_invocation.rs`; if it grows useful elsewhere, it can move into `tests/harness/mod.rs` later (not now — per "don't invent helpers before you need them").

Tests in this file:

| # | Test fn | Acceptance | Assertion shape |
|---|---|---|---|
| 1 | `lists_all_typed_tasks` | smoke | `rnme list` in the fixture root prints `caller_in_file`, `caller_cross_file`, etc. Verifies the workspace builds and inventory wires up. |
| 2 | `in_file_typed_call_runs_child_task` | §3 brief / plan §3 | `rnme run caller_in_file` succeeds; log output shows *two* distinct task starts (the caller and `root_noop`) — checked via a log substring count or by passing `--json` if available and counting `task_started` events. |
| 3 | `cross_file_typed_call_runs_descendant` | §4 brief / plan §4 | `rnme run caller_cross_file` succeeds; logs show the `deploy` body ran with the typed opts (the body logs the opts struct). |
| 4 | `dynamic_path_agrees_with_typed_path` | §7 brief / plan §7 | `rnme run caller_dynamic` succeeds; logs show the same `deploy` body ran. |
| 5 | `descendant_type_constructible_from_parent` | §6 brief / plan §5 | `rnme run caller_uses_child_type` succeeds. Compilation success alone is the proof — the struct path resolved. |
| 6 | `intermediate_runme_does_not_break_descendants` | plan §6 | `rnme list` shows `services/api_v2:deploy` and `structural_only/leaf:leaf_task` are both reachable. (The `services/` intermediate has its own RUNME.rs — its presence doesn't shadow `services/api/`.) |
| 7 | `rename_propagates_to_group_and_module` | plan §7 | (a) `rnme list` against the positive fixture lists `services/api_v2:deploy` (not `services/api:deploy`); (b) `rnme run services/api_v2:deploy` resolves and runs. This verifies the rename flows to the inventory group key and the on-disk dir name does *not* leak into the registered name. The module-path side is verified transitively by test 3 (`caller_cross_file` calls `subtasks::services::api_v2::deploy(...)` — if module-path propagation broke, that call wouldn't compile). |
| 8 | `rename_collision_is_rejected` *(negative)* | plan §8 / brief §8 | Marked `#[ignore]` until collision-detection lands. When enabled: `rnme list` against `typed_invocation_collision/` exits non-zero, stderr contains both `foo-bar/RUNME.rs` and `foo_bar/RUNME.rs` and the paste-ready `[rnme.rename]` snippet. |
| 9 | `rename_resolves_collision` | plan §391 / brief §5 | `rnme list` against `typed_invocation_collision_resolved/` succeeds and lists both `foo_bar_dashed:from_dashed_resolved` and `foo_bar:from_undered_resolved`. Exercises the rename-as-resolution path. Wired in this session if `apply-rename` is far enough along; otherwise `#[ignore]`'d with a TODO pointing at task #9 (apply-rename). |

The test file uses (or copies, as a small inline helper) the existing `harness::run_rnme()` pattern. The harness module currently lives in `tests/harness/mod.rs`; I'll re-use it via `mod harness;` at the top of `tests/typed_invocation.rs` (same pattern as other integration tests in this repo do — see `tests/cli_integration.rs:1-10`).

**Assertions are deliberately stdout/stderr-based, not registry-introspection-based.** Reasons:

- The integration test runs `rnme` as a subprocess; it doesn't have access to the in-process `Registry`.
- The fixture's value is end-to-end: macro emits → workspace generates → cargo builds → binary runs → child task starts → logs appear. A registry-level test would skip the codegen step that is the riskiest part of the plan.
- For finer-grained registry inspection, the existing `tests/dynamic_registration.rs` pattern is the right place — but this fixture is about the codegen path specifically.

---

## 4. Negative-case strategy

Three collision-related fixtures, three threats to `cargo test --workspace` going green throughout:

1. **`typed_invocation_collision/` fixture sources could break compilation** if any of them references a feature that doesn't exist yet (e.g., constructs a non-existent type). **Mitigated:** the collision-fixture RUNME.rs files only contain trivial tasks (Form-1, no cross-file calls, no `[rnme.rename]`). They're valid pre- and post-typed-shim.

2. **The collision test (`rename_collision_is_rejected`) would currently *fail*** because `rnme list` against `typed_invocation_collision/` succeeds today (collision-detection doesn't exist yet). **Mitigated:** the collision test starts life as `#[ignore]` with a TODO comment pointing at task #17 (`collision-detection`). When `impl-collision-detection` lands their work, they remove the `#[ignore]` as part of their validation.

3. **`typed_invocation_collision_resolved/` requires apply-rename plumbing** to actually distinguish the two siblings. Pre-`apply-rename`, both directories may still normalize to the same identifier (the rename string is parsed but not yet applied), in which case `rnme list` against the resolved fixture may *also* fail. **Mitigated:** test 9 (`rename_resolves_collision`) starts life as `#[ignore]` if `apply-rename` (task #9) is not yet validated by the time the fixture lands. It is un-`#[ignore]`'d by `impl-apply-rename` (or by re-invoking this task after task #9 completes).

This keeps `cargo test --workspace` green throughout. No separate test binary needed.

**Alternative considered and rejected:** putting the collision fixtures in their own test binary so the whole binary can be `#[ignore]`'d. Rejected because `#[ignore]` on individual test fns does the same job with one less file and one less `harness` import.

---

## 5. Phasing

The fixture lands incrementally as upstream tasks complete. Concrete state per phase:

### This session (Phase 2 start, alongside `typed-shim-macro`)

What gets wired in **immediately**, before `typed-shim-macro` is done:

- The full directory tree under all three fixtures: `testing/fixtures/typed_invocation/`, `testing/fixtures/typed_invocation_collision/`, `testing/fixtures/typed_invocation_collision_resolved/`.
- All RUNME.rs files with their *final* contents — including the typed in-file and cross-file calls, and the `[rnme.rename] name = "api_v2"` and `name = "foo_bar_dashed"` frontmatter. **These will not all compile against today's main** because `typed-shim-macro`, `subtasks-injection`, and `apply-rename` haven't landed. That's expected per the user's "broken between steps is fine" guidance.
- `tests/typed_invocation.rs` with all 9 tests defined, every one of them `#[ignore]`'d with a TODO breadcrumb pointing at the plan task that unblocks it. Mapping:
  - **Test 1 (`lists_all_typed_tasks`)** — *intended* as the in-this-session smoke test. In practice it cannot pass today either: the positive fixture's root `RUNME.rs` references `subtasks::services::api_v2::...` in its caller tasks, so the root crate fails to compile and no task — not even `root_noop` — is reachable through the fixture. Blocked on `subtasks-injection` (task #14). Un-`#[ignore]` once that lands.
  - **Tests 2, 3, 5** — `typed-shim-macro` (task #8) and/or `subtasks-injection` (task #14). Task #8 is already completed at fixture-landing time, but tests 3 and 5 use cross-file `subtasks::` paths so they also need #14.
  - **Tests 4, 6, 7, 9** — `apply-rename` (task #9) and/or `subtasks-injection` (task #14).
  - **Test 8** — `collision-detection` (task #17), per §4.

**Acceptance for this session:** the fixture trees exist, `tests/typed_invocation.rs` compiles (the test file itself is plain Rust — only the fixture RUNME.rs files have unresolved references, and those are only consumed at `rnme` invocation time), all 9 tests are `#[ignore]`'d with clear pointers, and `cargo test --workspace` passes (the ignored tests are skipped). `cargo build --workspace` is also clean (the fixture's RUNME.rs trees are *not* workspace members — they live under `testing/fixtures/` outside the workspace and are only compiled when `rnme` is run against them at test time).

**Note on the in-this-session passing test:** the brief asked for "the positive cases that don't depend on Phase 2/3 work" to be wired and passing in this session. After laying out the fixture per the approved layout, it turns out *every* positive case depends on subtasks-injection (Phase 3), because the root RUNME.rs uses `subtasks::` paths to demonstrate cross-file calls. The brief explicitly anticipated this outcome ("Cases that require typed-shim-macro stay broken until that lands — that's expected per the user's 'broken between steps is fine' guidance"), so I'm not adding extra in-this-session-only test scaffolding (e.g., a separate fixture that doesn't use `subtasks::`) just to have something green. Flagging in case the lead disagrees.

### After `apply-rename` lands (task #9 → val-rename #12)

Re-invoke this task (or `impl-apply-rename` does it inline as part of their validation). Un-`#[ignore]` tests 4, 7, and 9. Confirm they pass.

### After `typed-shim-macro` lands (Gate G2)

Re-invoke. Un-`#[ignore]` tests 2, 3, 5. Confirm they pass.

### After `subtasks-injection` lands (Gate G3)

Re-invoke. Un-`#[ignore]` test 6. Confirm it passes. (Note: tests 3, 5, 7 all already use `subtasks::` paths in their callers — if `typed-shim-macro` somehow lands before `subtasks-injection`, those tests can't actually compile against the fixture. The phase ordering in the plan puts `typed-shim-macro` at Phase 2 and `subtasks-injection` at Phase 3, so the expected sequence is: shim macro → cross-file paths start compiling → un-`#[ignore]` proceeds.)

### After `collision-detection` lands (task #17)

Re-invoke. Un-`#[ignore]` test 8.

---

## 6. What this task does *not* do

- Does **not** add the fixture's RUNME.rs files as cargo workspace members. They are intentionally outside the workspace; rnme reads them at test time.
- Does **not** generate the fixture from a builder helper. The files are checked in. Reviewers should be able to see the exact RUNME.rs contents in the PR diff.
- Does **not** invent helpers in rnme to make the fixture easier to write. Per the brief's "decision discipline" — uses the rnme surface as-is.
- Does **not** add a `#[cfg(feature = "fixture-typed-invocation")]` cargo feature. The `#[ignore]` mechanism is simpler and matches what test-audit will do for other affected tests.

---

## 7. Open questions for the reviewer

1. **Fixture location:** `testing/fixtures/` vs `tests/fixtures/`. I picked `testing/fixtures/` to keep `tests/` reserved for Rust integration-test entry points. Confirm or flip.
2. **One test file or many:** I picked one (`tests/typed_invocation.rs`). The collision case lives in it as a single `#[ignore]`'d fn. Alternative: split positive and negative into two files. I think one is fine for ~8 tests.
3. **`services/api/RUNME.rs` rename being a no-op:** the brief asks for `[rnme.rename]` to exercise the normalization-sibling case (`foo-bar` + `foo_bar`). The collision *fixture* covers the sibling-pair scenario (negative case). The positive fixture's rename is *just to verify the rename frontmatter parses and flows through*. If you'd prefer the positive fixture to actually rename to a different identifier (e.g., directory `api/` with `name = "api_v2"`), I'll change it. The non-no-op version exercises more plumbing.
4. **Logs-as-assertions:** tests 2/3/4 verify "the child task ran" by grepping for substrings the body logs. If you'd rather have a more structured assertion (e.g., a JSON event stream from `rnme run --json`), let me know — but that may require a CLI flag that doesn't exist today, which conflicts with "don't invent new framework features to make the fixture easier to write."
5. **`testing/fixtures/` being read-only at test time:** tests read from `CARGO_MANIFEST_DIR/testing/fixtures/...` directly, without copying to a temp dir. This means rnme's workspace cache lands in `testing/fixtures/typed_invocation/.rnme/` (or wherever rnme stashes it) — which I'll add to `.gitignore`. Alternative: copy the fixture to a `TempDir` for every test. Slower, but isolates runs. I picked direct-read for speed. Confirm or flip.

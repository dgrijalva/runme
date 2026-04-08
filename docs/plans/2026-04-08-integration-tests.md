# Integration Test Suite

## Goal

Build a comprehensive integration test suite for runme that validates the system end-to-end through two layers:

1. **In-process tests** — Define tasks with `#[runme::task]` in test crates, execute via `Registry`, assert on results. Fast, no compilation overhead.
2. **CLI subprocess tests** — Run the `runme` binary against fixture directories, assert on stdout/stderr/exit codes. Full pipeline coverage.

Plus two special-case explorations:
- **Shared crate tasks** — Verify that a shared library crate can define tasks visible to multiple RUNME.rs files
- **Dynamic task registration** — Design and prototype a mechanism for runtime task creation (e.g., a cargo plugin that auto-discovers and registers commands)

## Approach

**Layered testing:**
- In-process layer uses `Registry::from_inventory()` + `run_with_args()` (proven pattern from `task_args.rs`)
- CLI layer uses `--ui agent --format json` for structured assertions, `--ui cli` for output format tests
- Test helpers are shared utilities used by both layers (not a competing approach)

**Fixture isolation:**
- In-process fixtures: regular `.rs` test files with `#[runme::task]` + `const __RUNME_GROUP: &str = "";` — invisible to runme discovery
- CLI fixtures: temp directories created at test time with RUNME.rs files written programmatically — no pollution of project tree

**Special cases inform design:**
- Shared crate tasks should "just work" via inventory — test validates this
- Dynamic registration needs an InitContext extension — design spike with prototype

## Acceptance Criteria

- [ ] `cargo test` passes with all new integration tests
- [ ] In-process tests cover: execution (success/failure/exit codes), all 3 argument forms, task dependencies, output buffer capture, tracing integration
- [ ] CLI tests cover: discovery, group resolution, `--ui cli` and `--ui agent` modes, `--format text` and `--format json`, exit code propagation, error messages
- [ ] Test helper module provides reusable assertion utilities for both layers
- [ ] Shared crate test demonstrates cross-crate task visibility via inventory
- [ ] Dynamic registration design documented with working prototype
- [ ] No test fixtures appear in normal `runme` discovery from project root

## Human Review Gates

1. **Post-research synthesis** — Review research findings before committing to implementation approach. `Human Review: true, Auto-Approve: false`. *Rationale: foundational design decisions that shape all subsequent work.*
2. **Dynamic registration design** — Review proposed InitContext extension before implementation. `Human Review: true, Auto-Approve: false`. *Rationale: runtime API change that affects the public interface.*
3. **Final validation** — Review test results. `Auto-Approve: true`. *Rationale: mechanical verification, all criteria are testable.*

## Status

`draft`

## Context

### Key Files
- `crates/runme/tests/task_args.rs` — Existing in-process test pattern (proven)
- `crates/runme/src/task.rs` — Registry, TaskContext, TaskDef, TaskFn
- `crates/runme/src/cli.rs` — CLI dispatch, RunmeArgs, agent/cli modes
- `crates/runme/src/error.rs` — TaskError, exit code mapping
- `crates/runme/src/init.rs` — InitContext, InitDef (needs extension for dynamic reg)
- `crates/runme-macros/src/lib.rs` — `#[runme::task]` and `#[runme::init]` proc macros
- `crates/runme-cli/src/codegen.rs` — Generated runner main, `__runme_link()` pattern
- `crates/runme-cli/src/discover.rs` — RUNME.rs file discovery
- `docs/examples/RUNME.rs` — Comprehensive feature examples

### Design Constraints
- Rust edition 2024, nightly toolchain may be required
- `inventory` crate handles static registration — cross-crate works if linker retains symbols
- `__RUNME_GROUP` constant required by macro — set to `""` in standalone test files
- CLI binary is `runme` (built from `crates/runme-cli/`)

---

## Team

| Name | Role | Agent Type | Model | Strategy |
|------|------|-----------|-------|----------|
| inventory-researcher | Research inventory cross-crate behavior in test binaries | Explore | sonnet | subagent |
| cli-testing-researcher | Evaluate Rust CLI testing approaches (assert_cmd, etc.) | Explore | sonnet | subagent |
| coverage-auditor | Catalog existing test coverage, identify gaps | Explore | sonnet | subagent |
| research-synthesizer | Consolidate research findings, recommend approach | general-purpose | opus | subagent |
| foundation-builder | Implement test helpers and fixture infrastructure | general-purpose | opus | subagent |
| in-process-tester | Write in-process integration tests | general-purpose | opus | subagent |
| cli-tester | Write CLI subprocess integration tests | general-purpose | opus | subagent |
| shared-crate-tester | Build and test shared task crate scenario | general-purpose | opus | subagent |
| dynamic-reg-designer | Design and prototype dynamic task registration | general-purpose | opus | subagent |
| test-validator | Run full test suite, verify acceptance criteria | general-purpose | sonnet | subagent |

---

## Phase 1: Research (parallel)

### Task: research-inventory-crosscrate
- **Assigned To:** inventory-researcher
- **Depends On:** none
- **Parallel:** yes
- **Human Review:** no
- **Description:** Investigate how the `inventory` crate behaves when tasks are defined in a library crate that a test binary depends on. Specifically:
  1. Read the inventory crate documentation and source to understand its linker-section approach
  2. Check whether `inventory::collect!` + `inventory::iter` work across crate boundaries in `#[test]` binaries vs integration test binaries
  3. Look at how `__runme_link()` forces symbol retention in the codegen runner — would this be needed in test binaries too?
  4. Check if there are known issues with inventory + LTO or dead-code elimination in test builds

  Key files: `crates/runme/src/task.rs` (line 74: `inventory::collect!(TaskDef)`), `crates/runme-cli/src/codegen.rs` (the `__runme_link()` pattern)

### Task: research-cli-testing
- **Assigned To:** cli-testing-researcher
- **Depends On:** none
- **Parallel:** yes
- **Human Review:** no
- **Description:** Evaluate approaches for testing CLI binaries in Rust:
  1. `assert_cmd` crate — features, ergonomics, maturity
  2. Raw `std::process::Command` — what would a minimal wrapper look like?
  3. `assert_fs` for temp directory fixtures
  4. How other Rust CLI tools (cargo, ripgrep, etc.) structure their integration tests
  5. Consider: the `runme` binary compiles RUNME.rs files, so CLI tests have a compilation step. How does this affect test performance? Should we pre-compile fixtures?

  Recommend: which approach best fits runme's needs (structured JSON output, exit code assertions, temp fixture dirs)?

### Task: research-coverage-gaps
- **Assigned To:** coverage-auditor
- **Depends On:** none
- **Parallel:** yes
- **Human Review:** no
- **Description:** Audit existing test coverage in the runme codebase:
  1. Catalog all `#[test]` and `#[tokio::test]` functions — what do they cover?
  2. Identify which features have NO test coverage
  3. Map features to the appropriate test layer (in-process vs CLI)
  4. Prioritize gaps by risk/importance

  Focus areas: task execution lifecycle, error propagation, CLI flag handling, discovery logic, init hooks, group resolution, task name disambiguation. Check all files under `crates/runme/src/` and `crates/runme-cli/src/` for `#[cfg(test)]` modules, and `crates/runme/tests/` for integration tests.

---

## Phase 2: Research Synthesis

### Task: synthesize-research
- **Assigned To:** research-synthesizer
- **Depends On:** research-inventory-crosscrate, research-cli-testing, research-coverage-gaps
- **Parallel:** no
- **Human Review:** yes (Gate 1)
- **Description:** Consolidate findings from all three researchers into a concrete implementation recommendation:
  1. Summarize inventory cross-crate findings — any surprises or blockers?
  2. Recommend CLI testing approach with rationale
  3. Present prioritized test coverage plan — what to test first, what can wait
  4. Identify any risks or design decisions that need resolution
  5. Propose the test helper API surface (what assertions are needed for both layers)

  Output a concise summary (not a new plan document) that the human can review before implementation begins.

---

## Phase 3: Foundation

### Task: build-test-infrastructure
- **Assigned To:** foundation-builder
- **Depends On:** synthesize-research
- **Parallel:** no
- **Human Review:** no
- **Plan Approval:** yes — propose file structure and helper API before implementing
- **Description:** Build the test infrastructure that both in-process and CLI tests will use:

  **Test helper module** (`crates/runme/tests/helpers/` or `crates/runme/tests/common/`):
  - Assertion helpers for TaskResult (success, specific error, exit code)
  - Output buffer inspection (collect entries, search for patterns)
  - For CLI layer: subprocess runner that captures stdout/stderr/exit code, with JSON parsing for agent mode
  - Temp directory setup for CLI fixture tests (create dir, write RUNME.rs content, return path)

  **In-process fixture tasks** (in test files, NOT in RUNME.rs files):
  - A set of `#[runme::task]` functions covering common patterns: success, failure with code, panic, output to stdout, structured logging, dependencies, arguments
  - These live in regular test `.rs` files with `const __RUNME_GROUP: &str = "";`

  **CLI fixture templates:**
  - String constants or helper functions that generate RUNME.rs content for CLI tests
  - Should cover: single file, multiple files in subdirectories, files with dependencies on each other

  Ensure: `cargo test` passes after this phase. The helpers should compile and the fixture tasks should be runnable even if no assertion tests exist yet.

---

## Phase 4: Core Tests (parallel)

### Task: write-in-process-tests
- **Assigned To:** in-process-tester
- **Depends On:** build-test-infrastructure
- **Parallel:** yes (with write-cli-tests)
- **Human Review:** no
- **Description:** Write in-process integration tests using the test helpers and fixture tasks. Cover:

  **Execution lifecycle:**
  - Task runs successfully, returns Ok(())
  - Task returns Err with specific exit code
  - Task that spawns processes via ctx.exec()
  - Task that uses ctx.spawn() for background processes
  - Output buffer captures process stdout/stderr

  **Dependencies:**
  - Task with depends_on runs after dependency
  - Dependency failure propagation
  - Cross-task invocation via ctx.run()

  **Arguments (extend existing task_args.rs):**
  - Edge cases: empty strings, special characters, very long values
  - Help flag (--help) returns error with help text
  - Unknown flags rejected

  **Init hooks:**
  - Init function runs and can set group name
  - Multiple init hooks execute in order

  **Output and logging:**
  - Tracing macros (info!, error!) produce log entries
  - Output buffer ring behavior (capacity limits)
  - Log entry field extraction (timestamp, level, message)

  All tests should use `Registry::from_inventory()` and the test helper assertions. File location: `crates/runme/tests/` (one file per concern area, or a single `integration.rs` with modules).

### Task: write-cli-tests
- **Assigned To:** cli-tester
- **Depends On:** build-test-infrastructure
- **Parallel:** yes (with write-in-process-tests)
- **Human Review:** no
- **Description:** Write CLI subprocess integration tests. These run the actual `runme` binary against temp fixture directories. Cover:

  **Discovery:**
  - Single RUNME.rs file discovered and compiled
  - Nested RUNME.rs files produce correct group names
  - No RUNME.rs found → error message and non-zero exit

  **CLI modes:**
  - `--ui agent --format json`: structured JSON on success (`{"status":"ok","task":"name"}`)
  - `--ui agent --format json`: structured JSON on failure (`{"status":"error","task":"name","error":...}`)
  - `--ui agent --format text`: silent on success, "Error: ..." on failure
  - `--ui cli`: output forwarded to stdout/stderr

  **Exit codes:**
  - Success → 0
  - Task error → 1 (default) or specific code via ExitHint::Code(n)
  - Unknown task → 1

  **Task resolution:**
  - Short name resolution (root wins for collisions)
  - Qualified name (group:task) resolution
  - Unknown task name → helpful error message

  **Arguments forwarding:**
  - CLI args after task name forwarded to task parser
  - Task --help works through CLI

  Use temp directories with programmatically-generated RUNME.rs files. Clean up after each test. Consider test performance — each CLI test triggers a cargo build of the fixture.

---

## Phase 5: Special Cases

### Task: test-shared-crate-tasks
- **Assigned To:** shared-crate-tester
- **Depends On:** build-test-infrastructure
- **Parallel:** yes (with core tests)
- **Human Review:** no
- **Plan Approval:** yes — propose crate structure before implementing
- **Description:** Validate that tasks defined in a shared library crate are visible across multiple RUNME.rs files.

  **Setup:**
  Create a test scenario (could be in-process or a fixture directory) where:
  1. A shared library crate defines tasks with `#[runme::task]`
  2. Multiple "consumer" files/crates depend on the shared crate
  3. `Registry::from_inventory()` sees tasks from the shared crate

  **Questions to answer:**
  - Does inventory pick up tasks from transitive dependencies in test binaries?
  - Is `__runme_link()` needed, or do test binaries retain all symbols?
  - If `__runme_link()` IS needed, what's the user-facing pattern for shared task crates?
  - Can shared tasks have a different group than the consuming file?

  **Deliverable:** Working test demonstrating shared tasks, plus documentation of the pattern (as code comments, not separate docs).

### Task: design-dynamic-registration
- **Assigned To:** dynamic-reg-designer
- **Depends On:** research-inventory-crosscrate
- **Parallel:** yes (with other Phase 5 tasks)
- **Human Review:** yes (Gate 2)
- **Description:** Design and prototype a mechanism for dynamic task registration at startup.

  **Use case:** A "cargo" library crate that discovers available cargo subcommands at init time and registers a runme task for each one.

  **Current state:**
  - `InitContext` only has `set_group_name()` — no registry access
  - `Registry::register()` exists but isn't accessible during init
  - Init hooks run before `Registry::from_inventory()` in the generated runner (see `codegen.rs`)

  **Design space:**
  Option A: Add `register_task(TaskDef)` to `InitContext`. Init collects dynamic tasks, runner feeds them into Registry after init completes. Requires `TaskDef` to be constructable at runtime (currently uses `&'static str` fields — would need `Box::leak` or a separate DynamicTaskDef type).

  Option B: Separate registration phase. After init, before dispatch, run a `RegisterContext` callback that has `&mut Registry`. Cleaner separation but adds another hook type.

  Option C: Dynamic tasks register directly into inventory via `inventory::submit!` at init time — but this likely doesn't work since inventory is collected at link time, not runtime.

  **Deliverable:**
  1. Analysis of each option with tradeoffs
  2. Recommended approach
  3. Working prototype (can be rough — just needs to demonstrate the pattern)
  4. A simple test that dynamically creates and runs a task

---

## Phase 6: Validation

### Task: validate-test-suite
- **Assigned To:** test-validator
- **Depends On:** write-in-process-tests, write-cli-tests, test-shared-crate-tasks, design-dynamic-registration
- **Parallel:** no
- **Human Review:** auto-approve
- **Description:** Run the full test suite and verify all acceptance criteria:
  1. `cargo test --workspace` passes
  2. Check each acceptance criterion against the implemented tests
  3. Verify no test fixtures appear in `runme` discovery from project root (run `runme` from project root, confirm no test tasks appear)
  4. Review test output for flakiness (timing-dependent tests, resource contention)
  5. Report any failing or missing criteria

---

## Validation Profile

```yaml
validation:
  build:
    command: "cargo build --workspace"
    required: true
  tests:
    command: "cargo test --workspace"
    required: true
  clippy:
    command: "cargo clippy --workspace -- -D warnings"
    required: true
  no-fixture-leak:
    command: "# Run runme from project root, verify no test tasks in output"
    required: true
```

## Findings

### Research Synthesis (Task #4 — Human Review Gate)

**Three researchers completed parallel investigations. This synthesis consolidates their findings into an implementation recommendation.**

#### 1. Key Findings

**Inventory cross-crate behavior:** Works as expected for co-located tasks (same translation unit). Tasks from dependency crates get silently dropped by the linker unless you force symbol retention via the `__runme_link()` pattern already used in codegen. This is not a bug — it's how linker sections work. `#[used]` only prevents *compiler* elimination, not linker stripping. LTO is not a concern (constructor sections are linker roots). For integration tests, this means in-process tests that define tasks in the same test file work fine. A shared-crate test scenario will need to export and call `__runme_link()` in setup.

**CLI testing approach:** assert_cmd adds dependency weight for minimal gain since runme already outputs structured JSON. A thin custom wrapper (~50 lines) with JSON-aware assertions is a better fit. `tempfile::TempDir` (already a dev-dep) handles fixtures. `env!("CARGO_BIN_EXE_runme")` gives us the binary path automatically in integration tests under `crates/runme-cli/tests/`.

**Coverage audit:** ~350+ existing tests with strong coverage of task arguments, process management, output capture, log parsing, and TUI state. Critical gaps identified below.

**`depends_on` — the surprise finding:** The `depends_on` field is declared on `TaskDef`, populated by the `#[runme::task]` macro (parses `depends_on = "a,b,c"` attribute), and used in test fixture data (e.g., `depends_on: &["alpha"]` in task.rs tests). However, **no code anywhere reads or acts on this field at runtime**. Neither `Registry::run()`, the CLI dispatch, nor the TUI runner checks `depends_on` before executing a task. This is not a silent bug in the traditional sense — the data flows through correctly — but the feature is unimplemented. Dependencies are declared but never executed.

This needs a decision before we write tests for it: do we (a) implement dependency execution and then test it, (b) test that the field is populated correctly and document that execution is unimplemented, or (c) remove it? See Decisions section below.

#### 2. Recommended Testing Approach

**Two-layer strategy with performance-conscious CLI testing:**

| Layer | Location | What it covers | Cost per test |
|-------|----------|---------------|---------------|
| In-process | `crates/runme/tests/` | Task execution, args, output, init hooks, error propagation | ~0s (no compilation) |
| CLI subprocess | `crates/runme-cli/tests/` | Discovery, compilation pipeline, CLI flags, exit codes, UI modes | 1-30s (cargo build of fixture) |

**90%+ of tests should be in-process.** Reserve CLI subprocess tests for scenarios that genuinely require the full pipeline (discovery, compilation, CLI flag parsing).

**CLI test performance mitigation:** Use a single "kitchen sink" RUNME.rs fixture with multiple tasks covering success, failure, args, and groups. Write it to a stable path under `target/test-fixtures/` so it compiles once and caches across test runs. This keeps the CLI test count to ~5-10 true subprocess tests rather than 30+ with redundant compilations.

**Test helper API surface:**

```
// Shared assertions (both layers)
assert_task_ok(result: &TaskResult)
assert_task_err(result: &TaskResult, expected_code: Option<i32>)
assert_output_contains(buffer: &OutputBuffer, pattern: &str)
assert_output_entry_count(buffer: &OutputBuffer, expected: usize)

// CLI layer only
run_runme(args: &[&str], fixture_dir: &Path) -> CliOutput
assert_json_status(output: &CliOutput, status: &str)
assert_json_field(output: &CliOutput, field: &str, expected: &str)
assert_exit_code(output: &CliOutput, code: i32)
write_fixture(dir: &Path, relative_path: &str, content: &str)
```

#### 3. Prioritized Test Implementation Order

**Priority 1 — CRITICAL gaps (zero coverage, high risk):**
1. CLI dispatch (`cli.rs`: `run()`, `run_cli()`, `run_agent()`, `resolve_ui_mode()`) — this is the main entry point and completely untested
2. Exit code propagation end-to-end (TaskError -> process::exit)
3. Task returning `Err` through `Registry::run()` — basic failure path never tested
4. End-to-end compilation pipeline (compile.rs actually invoking `cargo build`)

**Priority 2 — HIGH gaps (feature areas with no coverage):**
5. Init hooks (`InitDef` collection, ordering, `#[runme::init]` macro exercised)
6. Cross-task invocation edge cases (transitive, with args, error propagation)
7. `depends_on` — pending decision on scope (see below)

**Priority 3 — MEDIUM gaps (secondary features):**
8. `--filter`, `--timeout` CLI flags
9. Watch mode end-to-end restart
10. Built-in `:list` task
11. Task name disambiguation via actual inventory

#### 4. Risks and Open Decisions

**DECISION NEEDED: `depends_on` scope**

Three options:
- **A) Implement + test:** Add dependency execution to `Registry::run()` (resolve deps, topological sort, run in order). Then write tests. This is new feature work, not just testing.
- **B) Test declaration only:** Verify the macro populates the field correctly. Document that execution is not yet implemented. Add a tracking comment or issue.
- **C) Remove:** Strip the field, macro attribute, and all references. Clean up dead code.

Recommendation: Option B for this test suite effort. The field and macro parsing work correctly — that's worth a quick test. Implementing dependency execution (Option A) is separate feature work that shouldn't be conflated with building the test suite. But this is your call.

**RISK: CLI test compilation time.** Each CLI subprocess test triggers a full cargo build of fixture RUNME.rs files. Mitigation (shared fixture, stable cache path) should keep this manageable, but if the build cache gets invalidated between runs, the CLI test suite could take 2-5 minutes. Worth monitoring.

**RISK: Nightly toolchain dependency.** Edition 2024 may require nightly. If CI uses stable, some tests may need feature gates.

**NOT A RISK: Inventory in test binaries.** Co-located tasks (defined in the test file itself) work reliably. The `__runme_link()` pattern is only needed for the shared-crate special case, which is an explicit Phase 5 task.

### Dynamic Task Registration Design (Task #9 — Human Review Gate)

**Prototype:** `crates/runme/tests/dynamic_registration.rs` — 9 passing tests demonstrating the pattern.

#### Analysis of Options

**Option A: Extend InitContext with task collection**
- Add `Vec<DynamicTaskDef>` to InitContext; init hooks call `ctx.register_task(...)`
- Runner drains collected tasks into Registry after all init hooks complete
- Pros: minimal new API surface, reuses existing hook mechanism, natural ordering
- Cons: InitContext gains a second responsibility; init is sync (discovery might want async)
- Complexity: low-medium

**Option B: Separate RegisterContext phase**
- New `#[runme::register]` hook type running after init with `&mut Registry`
- Pros: clean separation of concerns, could be async from day one
- Cons: new macro + inventory collection + codegen changes; two hooks per file for common case
- Complexity: medium-high

**Option C: Make Registry accept owned strings (DynamicTaskDef)**
- Registry stores both `&'static TaskDef` (from inventory) and owned `DynamicTaskDef`
- Pros: Registry is single source of truth; DynamicTaskDef uses owned Strings and closures naturally
- Cons: necessary infrastructure but insufficient alone (no discovery mechanism without a hook)
- Complexity: medium

#### Recommendation: A + C combined

Option C (DynamicTaskDef + Registry support) is necessary infrastructure regardless — you need owned strings and closures. Option A (InitContext collects tasks) is the simplest discovery hook, reusing an existing mechanism.

#### Key Design Decisions Proven in Prototype

1. **`DynamicTaskFn` uses `Arc<dyn for<'a> Fn(...)>`** — function pointers can't capture state, but dynamic tasks inherently need captured state (e.g., which subcommand to run). The HRTB `for<'a>` syntax is needed to match the existing TaskFn lifetime semantics.

2. **`DynamicTaskDef` uses owned `String` fields** — `&'static str` would require `Box::leak` for runtime-generated names, which permanently leaks memory. Owned strings are cleaner.

3. **Registry stores dynamic tasks in a separate `Vec<DynamicTaskDef>`** — lookup checks static tasks first, then dynamic. This avoids trait-object indirection on the common (static) path.

4. **Closures are `Fn` (not `FnOnce`)** — tasks can be re-run (e.g., watch mode). Prototype test `test_dynamic_task_rerunnable` validates this.

#### What the Prototype Covers

- Basic dynamic task creation and execution
- Captured state in closures (the "cargo subcommands" pattern)
- Error propagation from dynamic tasks
- Argument forwarding to dynamic tasks
- Coexistence of static (inventory) and dynamic tasks in one registry
- Task metadata (description, group) on dynamic tasks
- The InitContext collection pattern (simulated end-to-end runner lifecycle)

#### What's Needed for Real Integration

- Add `collected_tasks: Vec<DynamicTaskDef>` and `register_task()` method to `InitContext` in `crates/runme/src/init.rs`
- Move `DynamicTaskDef` and `DynamicTaskFn` into `crates/runme/src/task.rs`
- Extend `Registry` to hold `Vec<DynamicTaskDef>` alongside `Vec<&'static TaskDef>`
- Update `Registry::get()`, `resolve()`, `list()` to check both collections
- Update codegen in `crates/runme-cli/src/codegen.rs` to drain `InitContext.collected_tasks` into `Registry` after init hooks run
- Export new types through prelude

## Decisions Log

*(populated during execution)*

## Blockers

*(none identified yet)*

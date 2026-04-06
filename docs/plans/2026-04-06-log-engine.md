# Phase 5: Log Engine & Structured Output

## Goal

Build the log data system — parsing, filtering, composition, search, and streaming — so we understand the data layer before designing CLI APIs, agent mode, or TUI. At the end, the log engine is a self-contained library that can parse real-world process output into structured data, filter/search across it, compose multiple sources, and re-stream on demand.

## Approach

The existing foundation is solid: `LogLine` detects JSON, `OutputBuffer` is a ring buffer with broadcast channels, `TaskContext` captures per-task output. This plan extends that into a full log engine.

Strategy: **Research → Design (human review) → Implement in 3 waves → Validate**

The architecture uses a trait-based pipeline: `RecordParser` (splitting+parsing) → `FieldExtractor` (field mapping) → `LogEntry` (universal record). Autodetection by default, user-extensible via traits, overridable per-command via `Cmd`. See `docs/plans/2026-04-06-log-engine-design.md` for full design.

```
Research (parallel)
  ├── log-format-researcher:    field mappings, multiline patterns, format prevalence
  ├── filter-syntax-researcher: filter expression syntax for LogEntry
  └── codebase-researcher:      integration points for new trait-based pipeline
              │
     Design Synthesis ← HUMAN REVIEW
              │
     Wave 1: Parsing Pipeline (RecordParser + FieldExtractor + LogEntry)
              │
              ├── validate wave 1
              │
     Wave 2 (parallel):
              ├── filter-engine
              ├── log-store + multi-source composition
              └── search + context windows
              │
              ├── validate wave 2
              │
     Wave 3:
              ├── re-streaming / export
              │
         Final Validation
```

## Acceptance Criteria

- [ ] `RecordParser` trait with `feed()` / `flush()` / `reset()` API
- [ ] `FallbackParser` combinator (priority-ordered, handles Rejection/Incomplete backtracking)
- [ ] Built-in parsers: `JsonlParser`, `PlainLineParser` (at minimum)
- [ ] `FieldExtractor` trait with `extract()` API
- [ ] `LayeredExtractor` combinator (accumulation, all extractors run and merge)
- [ ] Built-in extractor: `CommonJsonFieldExtractor` (maps common field name variations)
- [ ] `LogEntry` struct with raw text, parsed content, well-known fields, and extension HashMap
- [ ] `Cmd` extension: `.record_parser()` and `.field_extractor()` for per-command overrides
- [ ] Autodetection works by default — no configuration needed for JSON or plain text
- [ ] Filter expressions parse and evaluate against `LogEntry` fields
- [ ] Filter is a view — underlying data is not modified
- [ ] Multiple task log sources can be composed into a single ordered stream
- [ ] Full-text search across captured output returns matching entries
- [ ] Context windows: N lines before/after matches
- [ ] Log stream can be re-streamed (subscribed to live, replayed from buffer)
- [ ] All new modules have unit tests
- [ ] Integration test: task produces mixed output → engine parses, filters, searches correctly
- [ ] `cargo test` passes across all crates

## Human Review Gates

1. **Design synthesis** — Review the final trait signatures, filter syntax, and integration plan.
   - Human Review: true
   - Auto-Approve: false
   - Rationale: This data layer shapes everything downstream. The trait boundaries and filter syntax are user-facing API decisions. The design doc draft exists but research may refine it.

2. **Wave 1 completion** — Review parsing pipeline (traits + implementations + LogEntry).
   - Human Review: true
   - Auto-Approve: true
   - Rationale: Foundational types. Auto-approvable if tests pass and types match design. Worth a glance.

3. **Wave 2 completion** — Review filter engine + log store + search.
   - Human Review: true
   - Auto-Approve: false
   - Rationale: The filter engine is a mini query language. The composition model determines how multi-task output works everywhere.

## Status

- [ ] Draft
- [ ] Approved
- [ ] Research complete
- [ ] Design synthesis complete
- [ ] Wave 1: Parsing pipeline
- [ ] Wave 2: Filter engine, log store, search
- [ ] Wave 3: Re-streaming, export
- [ ] Final validation

## Context

**Design decisions already made** (see `docs/plans/2026-04-06-log-engine-design.md`):
- Two core traits: `RecordParser` (splitting+parsing fused, stateful, streaming) and `FieldExtractor` (stateless, per-record)
- Composition via concrete combinators: `FallbackParser` (priority order) and `LayeredExtractor` (accumulation/merge)
- `LogEntry` struct: raw string, `ParsedContent` enum, well-known fields (timestamp, level, message as `Option<String>`), source tag, sequence number, extension `HashMap<String, Value>`
- Autodetection by default, override via `Cmd::record_parser()` / `Cmd::field_extractor()`
- Log level captured as raw string — no normalization or ranking yet
- Presentation traits are future work (multiple concerns: rendering, summarizing, diffing)
- String storage optimization (arena/range references) noted as future idea, not pursued now

**Existing code to extend:**
- `LogLine` enum (process.rs:19-42) — replaced by `LogEntry`
- `OutputBuffer` (process.rs:91-142) — evolves to hold `LogEntry`
- `TaskContext` (task.rs:42-85) — parser chain constructed here or in exec/spawn
- `Cmd` type (cmd.rs) — gains `.record_parser()` and `.field_extractor()` methods

**Testing approach:** Exercise through `TaskContext` directly. No CLI changes needed — existing task invocation is sufficient.

---

## Team

| Name | Role | Agent Type | Model | Strategy |
|------|------|-----------|-------|----------|
| log-format-researcher | Research field mappings, multiline patterns, format prevalence | Explore | opus | subagent |
| filter-syntax-researcher | Research filter expression syntax for LogEntry | Explore | opus | subagent |
| codebase-researcher | Map integration points for trait-based pipeline | Explore | opus | subagent |
| log-engine-architect | Finalize design from research + existing decisions | general-purpose | opus | subagent |
| parsing-pipeline-impl | Implement RecordParser, FieldExtractor, LogEntry, Cmd extension | general-purpose | opus | subagent |
| wave1-validator | Validate wave 1 output | general-purpose | sonnet | subagent |
| filter-engine-impl | Implement filter expression parser & evaluator | general-purpose | opus | subagent |
| log-store-impl | Implement log store and multi-source composition | general-purpose | opus | subagent |
| search-impl | Implement full-text search and context windows | general-purpose | opus | subagent |
| wave2-validator | Validate wave 2 output | general-purpose | sonnet | subagent |
| restream-impl | Implement re-streaming and export | general-purpose | opus | subagent |
| final-validator | Run full test suite and integration tests | general-purpose | sonnet | subagent |

---

## Phase 1: Research

### Task: research-log-formats

- **Depends On:** none
- **Assigned To:** log-format-researcher
- **Parallel:** yes
- **Human Review:** no
- **Description:**

The trait-based architecture is decided — research should focus on what the built-in implementations need to handle. Read the design doc at `docs/plans/2026-04-06-log-engine-design.md` first.

Investigate:
1. **JSON field name mappings.** Build a concrete table: what field names do these ecosystems use for level, message, and timestamp? Cover: Node.js (winston, pino, bunyan), Go (zap, zerolog, logrus), Python (structlog, stdlib json), Rust (tracing-subscriber JSON), Java (SLF4J/Logback JSON, Log4j2 JSON). This directly feeds `CommonJsonFieldExtractor`.

2. **Logfmt prevalence.** How common is logfmt (`key=value`) in practice? Is it worth a wave 1 parser or can we defer it? What ecosystems use it?

3. **Multiline patterns worth recognizing.** Specifically:
   - Rust panic output (`thread 'main' panicked at ...` + backtrace). What's the structure? How do you know when it ends?
   - Python tracebacks. What's the structure?
   - Cargo diagnostic output (errors with `-->` file references, indented help text).
   - Any other patterns a dev tool encounters frequently.
   - For each: what's the heuristic for detecting the start and end of the multiline record?

4. **Non-structured output that deserves attention.** The user notes that non-JSON text in a JSON stream often signals something important (panic, misconfiguration, debug output). What patterns indicate "this deserves highlighting"?

Deliverable: Field name mapping table, logfmt prevalence assessment, multiline pattern catalog with start/end heuristics.

### Task: research-filter-syntax

- **Depends On:** none
- **Assigned To:** filter-syntax-researcher
- **Parallel:** yes
- **Human Review:** no
- **Description:**

The filter operates on `LogEntry` — well-known fields (timestamp, level, message) plus an arbitrary `HashMap<String, Value>`. Read the design doc at `docs/plans/2026-04-06-log-engine-design.md` first.

Investigate:
1. **Syntax options.** Evaluate these against `LogEntry`'s actual structure:
   - Lucene-style: `level:error AND service:auth`
   - SQL WHERE-style: `level = 'error' AND service = 'auth'`
   - Simple key=value: `level=error service=auth` (implicit AND)
   - For each: how does it handle well-known fields vs extension fields? Nested access into `fields` HashMap? Regex matching? Numeric comparisons?

2. **Ergonomics for CLI typing.** Users type these in a terminal. Quoting, escaping, shell metacharacter conflicts matter. Which syntax has the least friction with shell escaping? (e.g., `level:error` is easier to type than `level='error'` in most shells)

3. **Parsing approach.** For the chosen syntax(es): hand-rolled recursive descent vs parser combinator crate (nom, winnow, pest, chumsky)? What's the right tool for this scope? We need good error messages for malformed expressions.

4. **Precedent review.** Look at how ripgrep, Grafana LogQL, Datadog, and Elasticsearch query strings handle this. What works well? What's overly complex?

Deliverable: Recommended syntax with examples showing common queries against LogEntry. Parser approach recommendation with rationale.

### Task: research-codebase-integration

- **Depends On:** none
- **Assigned To:** codebase-researcher
- **Parallel:** yes
- **Human Review:** no
- **Description:**

Map exactly how the new trait-based pipeline integrates with existing code. Read the design doc at `docs/plans/2026-04-06-log-engine-design.md` and then read these files thoroughly:
- `crates/runme/src/process.rs` — LogLine, OutputBuffer, exec(), spawn()
- `crates/runme/src/task.rs` — TaskContext
- `crates/runme/src/cmd.rs` — Cmd type

Answer these specific questions:
1. **Parser lifecycle.** Each `exec()` / `spawn()` call should get its own parser instance (since parsers are stateful). Where is the parser constructed? Options: (a) Cmd carries an optional `Box<dyn RecordParser>`, exec/spawn uses it or falls back to default. (b) TaskContext holds a parser factory. Which is cleaner?

2. **Field extractor lifecycle.** Extractors are stateless. Should there be one per TaskContext? One per exec call? A global default?

3. **OutputBuffer migration.** Currently `VecDeque<LogLine>`. What's the minimal change to store `LogEntry` instead? What breaks? List every use site of `LogLine` and `OutputBuffer` in the codebase.

4. **spawn() buffer isolation.** Currently `spawn()` creates a separate `Arc<Mutex<OutputBuffer>>`. Should spawned processes feed into the TaskContext's buffer? Or is separation correct (and composition handles merging)?

5. **Cmd extension.** The `Cmd` struct needs to carry optional `Box<dyn RecordParser>` and `Box<dyn FieldExtractor>`. What trait bounds are needed? (Send + Sync? Clone?) How does this interact with `From<&str>` and `From<std::process::Command>` conversions?

6. **Module layout.** Propose the exact file structure under `crates/runme/src/log/`. What goes in `mod.rs` vs separate files?

Deliverable: Integration plan with specific code locations, migration checklist, and proposed module layout.

---

## Phase 2: Design Synthesis

### Task: design-log-engine

- **Depends On:** research-log-formats, research-filter-syntax, research-codebase-integration
- **Assigned To:** log-engine-architect
- **Parallel:** no
- **Human Review:** true (Gate 1 — design review)
- **Description:**

The design doc at `docs/plans/2026-04-06-log-engine-design.md` already captures core architectural decisions. Your job is to **finalize it** using the research findings — not start from scratch.

Update the design doc with:
1. **Concrete field name mapping table** from log-format research → feeds into `CommonJsonFieldExtractor` implementation.
2. **Final list of wave 1 parsers** — JsonlParser and PlainLineParser are confirmed. Research will tell us whether to add logfmt, Rust panic, or others in wave 1.
3. **Chosen filter syntax** with full grammar, examples, and parser approach.
4. **Integration plan** — exact code changes, migration path for LogLine→LogEntry, parser/extractor lifecycle decisions.
5. **Any refinements to trait signatures** based on what research uncovered.

Do NOT change decisions already marked as decided in the design doc unless research reveals a concrete problem.

---

## Phase 3: Wave 1 — Parsing Pipeline

### Task: impl-parsing-pipeline

- **Depends On:** design-log-engine
- **Assigned To:** parsing-pipeline-impl
- **Parallel:** no (foundation for everything else)
- **Human Review:** no (covered by wave 1 validation gate)
- **Plan Approval:** yes — propose approach before implementing
- **Description:**

Implement the core parsing pipeline as specified in the finalized design document. This is the foundation everything else builds on.

Key work:
- `RecordParser` trait with `feed()` / `flush()` / `reset()` API
- `ParseResult` enum: `Record(RawRecord)` / `Rejection` / `Incomplete`
- `FallbackParser` combinator with backtracking logic
- Built-in parsers: `JsonlParser`, `PlainLineParser`, plus any others specified in design
- `FieldExtractor` trait with `extract()` API
- `LayeredExtractor` combinator with merge logic
- Built-in extractor: `CommonJsonFieldExtractor` with field name mapping table from design
- `LogEntry` struct as specified in design
- `Cmd` extension: `.record_parser()` and `.field_extractor()`
- Migrate `OutputBuffer` from `LogLine` to `LogEntry`
- Update `exec()` and `spawn()` to use the parsing pipeline
- Update `TaskContext` integration
- Migrate or update all existing tests that reference `LogLine` or `OutputBuffer`
- New tests: each parser, each extractor, FallbackParser behavior, LayeredExtractor merge, LogEntry construction, Cmd extensions, autodetection end-to-end

### Task: validate-wave1

- **Depends On:** impl-parsing-pipeline
- **Assigned To:** wave1-validator
- **Parallel:** no
- **Human Review:** true (Gate 2 — wave 1 review, auto-approvable)
- **Description:**

Validate wave 1:
1. `cargo test --workspace` passes
2. `RecordParser` and `FieldExtractor` trait signatures match design
3. `FallbackParser` handles Record/Rejection/Incomplete correctly
4. JSON autodetection works (JSON lines → Structured, plain text → Raw)
5. `CommonJsonFieldExtractor` maps field name variations correctly
6. `Cmd::record_parser()` and `Cmd::field_extractor()` overrides work
7. `OutputBuffer` stores `LogEntry` correctly
8. No regressions in existing process/task tests
9. Check the design doc against implementation — flag deviations

---

## Phase 4: Wave 2 — Filter, Store, Search

### Task: impl-filter-engine

- **Depends On:** validate-wave1
- **Assigned To:** filter-engine-impl
- **Parallel:** yes (with impl-log-store, impl-search)
- **Human Review:** no (covered by wave 2 validation gate)
- **Plan Approval:** yes
- **Description:**

Implement the filter expression engine as specified in the design document. Key work:
- Filter expression AST type
- Parser (expression string → AST) using approach specified in design
- Evaluator: `filter.matches(&LogEntry) -> bool`
- Operators on well-known fields (level, message, timestamp) and extension fields
- AND/OR/NOT composition
- Field comparison: =, !=, contains, regex match
- Good error messages for malformed expressions
- Comprehensive tests: parsing, evaluation, edge cases, error messages

The filter is a pure function. No side effects, no mutation of the log entry.

### Task: impl-log-store

- **Depends On:** validate-wave1
- **Assigned To:** log-store-impl
- **Parallel:** yes (with impl-filter-engine, impl-search)
- **Human Review:** no (covered by wave 2 validation gate)
- **Plan Approval:** yes
- **Description:**

Implement the log store and multi-source composition layer. Key work:
- Log store that extends or wraps `OutputBuffer` for richer capabilities
- Source tagging — each `LogEntry` already has `source`, store indexes by it
- Multi-source composition — combine logs from multiple tasks into a single ordered stream (using `seq` + `source` for ordering)
- Filtered views — apply a filter to produce a view without mutating underlying data
- Resolve spawn() buffer integration per the design document
- Live subscription with optional filter support
- Grouping by field value (by source, by level, by arbitrary field)
- Tests: composition ordering, filtered views, grouping, live subscription

Keep data structures simple for now. Linear scans are fine. We can optimize later if volume becomes an issue.

### Task: impl-search

- **Depends On:** validate-wave1
- **Assigned To:** search-impl
- **Parallel:** yes (with impl-filter-engine, impl-log-store)
- **Human Review:** no (covered by wave 2 validation gate)
- **Plan Approval:** yes
- **Description:**

Implement full-text search and context windows. Key work:
- Full-text search across `LogEntry` (raw text and message field)
- Regex search support
- Context windows: return N entries before/after each match
- Search result type: matching entry + context + match metadata (position, source)
- Search across single source or composed multi-source view
- Tests: text search, regex, context windows, multi-source search

### Task: validate-wave2

- **Depends On:** impl-filter-engine, impl-log-store, impl-search
- **Assigned To:** wave2-validator
- **Parallel:** no
- **Human Review:** true (Gate 3 — wave 2 review)
- **Description:**

Validate wave 2:
1. `cargo test --workspace` passes
2. Filter expressions parse and evaluate correctly per design
3. Log store composes multiple sources with correct ordering
4. Filtered views work without mutating underlying data
5. Search returns correct results with context windows
6. Grouping by field value works
7. All three modules integrate cleanly with wave 1 types
8. No regressions

---

## Phase 5: Wave 3 — Re-streaming & Export

### Task: impl-restream

- **Depends On:** validate-wave2
- **Assigned To:** restream-impl
- **Parallel:** no
- **Human Review:** no (covered by final validation)
- **Plan Approval:** yes
- **Description:**

Implement re-streaming and export capabilities. Key work:
- Live tailing: subscribe to a log store/view and receive new entries as they arrive
- Replay: re-stream historical entries from the buffer
- Filtered streaming: apply a filter to a live stream
- Export: dump log entries to a `Write` impl (for future file/stdout/pipe output)
- Format options: raw text, JSON lines (at minimum)
- Integration with the existing broadcast channel pattern
- Tests: live streaming, replay, filtered streaming, export formats

### Task: final-validation

- **Depends On:** impl-restream
- **Assigned To:** final-validator
- **Parallel:** no
- **Human Review:** no
- **Description:**

Final validation:
1. `cargo test --workspace` passes
2. `cargo clippy --workspace -- -D warnings` passes
3. Integration test: create TaskContext, run processes that produce mixed output (JSON, plain text, maybe logfmt), exercise full pipeline: parse → extract → filter → search → compose → re-stream
4. Verify all acceptance criteria from the top of this plan
5. Check existing RUNME.rs example still works
6. Report results with pass/fail for each criterion

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
```

## Decisions Log

| Decision | Rationale | Date |
|----------|-----------|------|
| Two core traits: RecordParser + FieldExtractor | Splitting/parsing are coupled (format-dependent), field extraction is separate (same format, different naming). Traits for user extensibility. | 2026-04-06 |
| Composition via concrete combinators, not generic chain | FallbackParser (priority) and LayeredExtractor (accumulation) match the natural shape of their domains. No framework needed. | 2026-04-06 |
| LogEntry with well-known fields + extension HashMap | Typed core fields for filtering/sorting, flexible extension for everything else. Graceful degradation — missing fields are just None. | 2026-04-06 |
| Autodetect by default, override via Cmd | Keeps RUNME.rs lean. Override is per-command (not per-task) because one task may run multiple commands with different formats. | 2026-04-06 |
| Log level as raw string, no normalization | Different commands use different level systems. Normalize later with extensible defaults. Not load-bearing for the data system. | 2026-04-06 |
| String storage optimization deferred | Owned strings for now. Arena/range-reference approach noted for future if allocation pressure becomes measurable. | 2026-04-06 |
| Presentation traits are future work | Multiple concerns (rendering, summarizing, diffing). Design once data model is proven. | 2026-04-06 |
| Simple data structures, optimize later | Linear scans for filter/search. Dev tool session volume (~100k lines) doesn't warrant upfront indexing. | 2026-04-06 |

## Findings

(Populated during research phase)

## Blockers

(None identified yet)

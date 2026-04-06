# Log Engine Design

Status: **Draft** — captures decisions from design discussion, open questions for research to resolve.

## Core Architecture

The log engine is a pipeline with user-extensible stages:

```
stdout/stderr bytes
       │
       ▼
  RecordParser  (trait — splitting + parsing fused)
       │         FallbackParser tries parsers in priority order
       │         emits: Record | Rejection | Incomplete
       ▼
  FieldExtractor  (trait — stateless, per-record)
       │            LayeredExtractor runs all extractors, merges results
       ▼
  LogEntry  (struct — the universal log record)
       │
       ▼
  Storage / Querying / Presentation (downstream consumers)
```

### Design Principles

- **Autodetection by default.** The engine figures out the format. RUNME.rs files stay lean.
- **User-extensible via traits.** Built-in implementations for common formats, but users can supply their own `RecordParser` and `FieldExtractor` implementations.
- **Override via Cmd.** The `Cmd` builder carries optional hints when autodetection isn't enough. Hints live on Cmd (not on the task) because a single task may run multiple commands with different output formats.
- **Graceful degradation.** If parsing fails, you get a raw text entry with no extracted fields. Everything still works — filtering, search, display.

---

## RecordParser Trait

Fuses record splitting and parsing. These are coupled because you can't split records without understanding the format (JSON needs brace-depth tracking, plain text splits on newlines, multiline patterns need continuation detection).

```rust
pub enum ParseResult {
    /// Successfully parsed a complete record
    Record(RawRecord),
    /// This parser doesn't handle this input — try the next one
    Rejection,
    /// Input could be a partial record in this format — need more data
    Incomplete,
}

/// A raw record as produced by a parser, before field extraction.
pub struct RawRecord {
    pub raw: String,
    pub parsed: ParsedContent,
}

pub enum ParsedContent {
    Json(serde_json::Value),
    // Future: Logfmt(Vec<(String, String)>), etc.
    PlainText,
}

pub trait RecordParser: Send + Sync {
    /// Feed a line (or chunk) of text. Returns a parse result.
    /// Parsers may be stateful (buffering partial records).
    fn feed(&mut self, line: &str) -> ParseResult;

    /// Flush any buffered partial input (e.g., at stream end).
    /// Returns a record if there was buffered content, None otherwise.
    fn flush(&mut self) -> Option<RawRecord>;

    /// Reset parser state (e.g., between commands).
    fn reset(&mut self);
}
```

### Composition: FallbackParser

Priority-ordered fallback. Tries each inner parser in order. First `Record` wins. `Incomplete` means "buffer more and keep trying this parser." `Rejection` means "try the next parser."

```rust
pub struct FallbackParser {
    parsers: Vec<Box<dyn RecordParser>>,
}

impl FallbackParser {
    pub fn new(parsers: Vec<Box<dyn RecordParser>>) -> Self;
}

impl RecordParser for FallbackParser { ... }
```

### Built-in Parsers

- **JsonlParser** — Detects JSON objects and arrays. Single-line by default. Handles the common case of structured log output.
- **PlainLineParser** — Always succeeds. One line = one record. Terminal fallback.

**Open for research:**
- Do we need a multiline JSON parser (pretty-printed JSON)?
- Should we have a RustPanicParser that recognizes `thread 'main' panicked at ...` and captures the full backtrace as one record?
- Logfmt parser — how common is this in practice? Worth building for wave 1?
- What other patterns are worth recognizing? (Python tracebacks, cargo diagnostics, etc.)

### Default Parser Chain

```rust
FallbackParser::new(vec![
    Box::new(JsonlParser),
    // Future: Box::new(RustPanicParser),
    // Future: Box::new(LogfmtParser),
    Box::new(PlainLineParser),  // always succeeds, terminal
])
```

---

## FieldExtractor Trait

Stateless, per-record. Takes a parsed record and populates well-known fields on the LogEntry. Separate from parsing because the same format (e.g., JSON) can have completely different field naming conventions.

```rust
pub trait FieldExtractor: Send + Sync {
    /// Extract well-known fields from a parsed record.
    /// Returns the fields it found. Missing fields are simply absent.
    fn extract(&self, record: &RawRecord) -> ExtractedFields;
}

pub struct ExtractedFields {
    pub timestamp: Option<String>,  // raw string for now, normalize later
    pub level: Option<String>,      // raw string, no normalization yet
    pub message: Option<String>,
    pub fields: HashMap<String, serde_json::Value>,
}
```

### Composition: LayeredExtractor

Unlike parsers (where one wins), extractors accumulate. All run, results merge. This is because different extractors look at different parts of the data — one finds `level`/`message`, another finds `trace_id`/`span_id`.

```rust
pub struct LayeredExtractor {
    extractors: Vec<Box<dyn FieldExtractor>>,
}

impl LayeredExtractor {
    pub fn new(extractors: Vec<Box<dyn FieldExtractor>>) -> Self;
}

impl FieldExtractor for LayeredExtractor { ... }
```

Merge strategy: later extractors do not overwrite fields set by earlier ones (first writer wins for well-known fields; `fields` HashMap merges all).

### Built-in Extractors

- **CommonJsonFieldExtractor** — Maps common JSON field names to well-known fields. Handles the `level`/`severity`/`lvl` and `msg`/`message` and `ts`/`timestamp`/`time` variations.

**Open for research:**
- What field name mappings are most common across ecosystems? (Node, Go, Python, Rust/tracing, Java/SLF4J)
- Are there other well-known fields beyond timestamp/level/message worth extracting into the struct? (service, trace_id, request_id?)
- Should there be a logfmt-specific extractor or does the generic JSON one cover it once logfmt is parsed into key-value pairs?

---

## LogEntry Struct

The universal log record. Everything downstream (filtering, search, display, export) works with this type.

```rust
pub struct LogEntry {
    /// The raw text of the record, exactly as captured from the process
    pub raw: String,
    /// How the record was parsed
    pub parsed: ParsedContent,
    /// Which task/command produced this entry
    pub source: String,
    /// Sequence number (monotonic within a source)
    pub seq: u64,

    // Well-known fields (populated by FieldExtractor, all optional)
    pub timestamp: Option<String>,  // raw string for now
    pub level: Option<String>,      // raw string, no ranking/normalization
    pub message: Option<String>,

    /// Additional extracted fields
    pub fields: HashMap<String, serde_json::Value>,
}
```

### Design Decisions

- **Level is a raw string.** No normalization or ranking for now. Different commands use different level systems. We'll add a useful default ranking with user extensibility later, but it's not load-bearing for the data system.
- **Timestamp is a raw string.** Parsing into DateTime is a future concern. Capture accurately first.
- **`source` identifies the producing command.** Set by the execution layer, not by the parser/extractor.
- **`seq` provides ordering.** Monotonic per source. Combined with source, gives a total order for composition.

### Future: String Storage Optimization

Currently `raw` is an owned `String`. With hundreds of thousands of entries, this means hundreds of thousands of small allocations. A future optimization could store raw output in a contiguous backing buffer and use byte-range references instead of owned strings. Not worth pursuing until it's a measured bottleneck — noting it here so the API doesn't preclude it.

---

## Cmd Extension

The `Cmd` type (from Phase 3b) gains optional log engine configuration:

```rust
impl Cmd {
    /// Override the default parser chain for this command's output
    pub fn record_parser(self, parser: impl RecordParser + 'static) -> Self;

    /// Override the default field extractor for this command's output
    pub fn field_extractor(self, extractor: impl FieldExtractor + 'static) -> Self;
}
```

When not set, the default `FallbackParser` and `LayeredExtractor` are used (autodetection). This keeps RUNME.rs files lean by default while allowing full control when needed:

```rust
#[runme::task]
async fn run_weird_app(ctx: &TaskContext) -> TaskResult {
    // This app has a custom log format
    ctx.exec(
        Cmd::new("weird-app")
            .record_parser(MyCustomParser::new())
            .field_extractor(MyCustomExtractor::new())
    ).await?;
    Ok(())
}
```

---

## Presentation (Future Work)

Multiple traits will be needed for presenting log data. Not designed yet — waiting until the data model is solid. Known concerns:

- Rendering a single entry as text (terminal, export, JSON lines)
- Rendering a stream/view (headers, separators, color, interleaving markers)
- Summarizing (counts by level, by source — what agent mode would want)
- Diffing/highlighting (what changed between runs)

The `LogEntry` struct should carry enough information to support all of these. If we discover it doesn't, that's a signal to revisit the struct.

---

## Open Questions for Research

### For log-format-researcher:
1. What JSON field name mappings are most common? Build a table across Node, Go, Python, Rust/tracing, Java ecosystems.
2. How common is logfmt in practice? Worth a wave 1 parser?
3. What multiline patterns are worth recognizing? (Rust panics, Python tracebacks, Java stack traces, cargo diagnostics)
4. Are there formats beyond JSON/logfmt/plain-text that a dev tool encounters regularly?

### For filter-syntax-researcher:
1. Given that filters operate on `LogEntry` (well-known fields + arbitrary `fields` map), what syntax is most ergonomic for CLI usage?
2. How should nested field access work for the `fields` HashMap? (dot notation? bracket notation?)
3. What Rust parsing crate (if any) is appropriate, or is hand-rolled better for this scope?

### For codebase-researcher:
1. How should `RecordParser` state be managed per-command within a task? (Each `exec()` call gets its own parser instance? Shared across the task?)
2. Where does the parser chain get constructed? In `exec()`/`spawn()` from the Cmd's hints + defaults?
3. How does the spawn() buffer isolation map to this new architecture? Does `spawn()` feed into the same log store as `exec()`?
4. What's the minimal change to `OutputBuffer` to store `LogEntry` instead of `LogLine`?

---

## Module Layout (Proposed)

```
crates/runme/src/
├── log/
│   ├── mod.rs          — re-exports, LogEntry struct
│   ├── parse.rs        — RecordParser trait, FallbackParser, built-in parsers
│   ├── extract.rs      — FieldExtractor trait, LayeredExtractor, built-in extractors
│   ├── filter.rs       — filter expression engine (wave 2)
│   ├── store.rs        — log store, multi-source composition (wave 2)
│   ├── search.rs       — full-text search, context windows (wave 2)
│   └── stream.rs       — re-streaming, export (wave 3)
├── ...existing files...
```

`LogLine` in process.rs is replaced by the new `LogEntry` type. `OutputBuffer` either evolves to hold `LogEntry` or is wrapped by the log store.

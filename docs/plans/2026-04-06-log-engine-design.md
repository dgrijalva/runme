# Log Engine Design

Status: **Final** — all research questions resolved, ready for human review.

## Core Architecture

The log engine is a pipeline with user-extensible stages:

```
stdout/stderr bytes
       |
       v
  RecordParser  (trait -- splitting + parsing fused)
       |         FallbackParser tries parsers in priority order
       |         emits: Record | Rejection | Incomplete
       v
  FieldExtractor  (trait -- stateless, per-record)
       |            LayeredExtractor runs all extractors, merges results
       v
  LogEntry  (struct -- the universal log record)
       |
       v
  Storage / Querying / Presentation (downstream consumers)
```

### Design Principles

- **Autodetection by default.** The engine figures out the format. RUNME.rs files stay lean.
- **User-extensible via traits.** Built-in implementations for common formats, but users can supply their own `RecordParser` and `FieldExtractor` implementations.
- **Override via Cmd.** The `Cmd` builder carries optional hints when autodetection isn't enough. Hints live on Cmd (not on the task) because a single task may run multiple commands with different output formats.
- **Graceful degradation.** If parsing fails, you get a raw text entry with no extracted fields. Everything still works -- filtering, search, display.

---

## RecordParser Trait

Fuses record splitting and parsing. These are coupled because you can't split records without understanding the format (JSON needs brace-depth tracking, plain text splits on newlines, multiline patterns need continuation detection).

The parser operates on raw bytes, not pre-split lines. This supports binary formats, concatenated JSON, and any framing scheme. The **stream handler** owns the accumulation buffer (`BytesMut`); the parser is a pure recognizer that scans from the start of the buffer and reports how many bytes it consumed.

```rust
pub enum ParseResult {
    /// Successfully parsed a complete record. `usize` is bytes consumed from input.
    Record(RawRecord, usize),
    /// This parser doesn't handle this input -- try the next one
    Rejection,
    /// Input could be a partial record in this format -- need more data
    Incomplete,
}

/// A raw record as produced by a parser, before field extraction.
pub struct RawRecord {
    pub raw: String,
    pub parsed: ParsedContent,
}

pub enum ParsedContent {
    Json(serde_json::Value),
    Logfmt(Vec<(String, String)>),
    PlainText,
}

pub trait RecordParser: Send + Sync {
    /// Scan the front of `data` for a record.
    ///
    /// - `data`: the accumulated bytes not yet consumed. The parser examines
    ///   the beginning of this slice and returns how many bytes it consumed.
    /// - `eof`: true when no more data will arrive (process exited). Parsers
    ///   that would normally return `Incomplete` should emit what they have
    ///   or `Rejection` so the next parser can try.
    ///
    /// Parsers do **not** buffer data internally -- the caller owns the buffer
    /// and re-feeds the full unconsumed slice on each call.
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult;

    /// Reset parser state (e.g., between commands).
    fn reset(&mut self);
}
```

### Stream Handler

The stream handler in `exec()` / `spawn()` owns a `BytesMut` accumulation buffer and drives the parser:

```rust
use bytes::BytesMut;

let mut buf = BytesMut::new();
loop {
    // read new bytes from stdout/stderr into buf
    let n = reader.read_buf(&mut buf).await?;
    let eof = n == 0;

    // drain records from the buffer
    loop {
        match parser.feed(&buf, eof) {
            Record(record, consumed) => {
                buf.advance(consumed);
                let entry = build_log_entry(record, &extractor, source, &mut seq);
                output_buffer.push(entry);
                // continue -- buffer may contain more records
            }
            Incomplete => break,  // wait for more data
            Rejection => break,   // shouldn't happen with FallbackParser
        }
    }
    if eof { break; }
}
```

### Composition: FallbackParser

Priority-ordered fallback. Tries each inner parser in order. First `Record` wins. `Incomplete` means "buffer more and keep trying this parser." `Rejection` means "try the next parser."

Tracks an "active parser" index. When a parser returns `Incomplete`, it becomes active and is tried exclusively on subsequent calls until it either produces a `Record` (active cleared, restart from top) or `Rejection` (active cleared, try next parser).

```rust
pub struct FallbackParser {
    parsers: Vec<Box<dyn RecordParser>>,
    active: Option<usize>,  // index of parser that returned Incomplete
}

impl FallbackParser {
    pub fn new(parsers: Vec<Box<dyn RecordParser>>) -> Self;
}

impl RecordParser for FallbackParser {
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult {
        if let Some(idx) = self.active {
            match self.parsers[idx].feed(data, eof) {
                Record(rec, n) => { self.active = None; return Record(rec, n); }
                Incomplete => return Incomplete,
                Rejection => { self.active = None; /* fall through to try next */ }
            }
        }
        let start = self.active.map(|i| i + 1).unwrap_or(0);
        self.active = None;
        for i in start..self.parsers.len() {
            match self.parsers[i].feed(data, eof) {
                Record(rec, n) => return Record(rec, n),
                Incomplete => { self.active = Some(i); return Incomplete; }
                Rejection => continue,
            }
        }
        Rejection
    }
}
```

### Built-in Parsers (Wave 1)

All parsers receive `&[u8]` and return bytes consumed. Text parsers validate UTF-8 from the byte slice for the `RawRecord.raw` field. Binary format support (BSON, protobuf) is a future extension — the trait boundary already supports it.

- **JsonlParser** -- Scans for JSON objects/arrays at the start of the buffer. Uses brace/bracket depth tracking (skipping over quoted strings) to find the end of the record. Handles both newline-delimited JSON and concatenated JSON (`{"a":1}{"b":2}`). Bytes consumed includes the record and any trailing newline/whitespace. Anomaly detection: when the parser has seen >3 JSON records and encounters non-JSON, it rejects (so the next parser handles it) but sets a flag for the field extractor. At EOF, emits partial JSON as PlainText or rejects.
- **RustPanicParser** -- Scans for the start pattern `thread '...' panicked at` in the buffer. If found, scans continuation lines. Returns Record when the first non-continuation line is found (consumed bytes include only the panic, not the trailing non-continuation line). At EOF, emits whatever it has.
- **CargoDiagnosticParser** -- Scans for cargo `error`/`warning` diagnostic start. Captures the full diagnostic block. Similar to RustPanicParser.
- **LogfmtParser** -- Scans for a newline-terminated line containing `key=value` pairs. Validates the logfmt structure. Bytes consumed includes the trailing newline. At EOF, emits the remaining buffer if it looks like logfmt.
- **PlainLineParser** -- Scans for `\n`. Returns everything up to and including `\n` as one record (trailing newline stripped from `raw`). At EOF, emits whatever remains. Always succeeds — terminal fallback.

**Deferred to wave 2:** Python tracebacks, Java stack traces, Go panics, Node.js multi-line errors. All have been cataloged with heuristics by research but are lower priority than the Rust-ecosystem patterns above.

### Default Parser Chain

```rust
FallbackParser::new(vec![
    Box::new(JsonlParser::new()),
    Box::new(RustPanicParser::new()),
    Box::new(CargoDiagnosticParser::new()),
    Box::new(LogfmtParser::new()),
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

Unlike parsers (where one wins), extractors accumulate. All run, results merge. This is because different extractors look at different parts of the data -- one finds `level`/`message`, another finds `trace_id`/`span_id`.

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

- **CommonJsonFieldExtractor** -- Maps common JSON field names to well-known fields. Works with both `ParsedContent::Json` and `ParsedContent::Logfmt` (same field naming conventions apply). No separate logfmt extractor needed.

### Field Name Mapping Table

The `CommonJsonFieldExtractor` checks field names in priority order (first match wins). This table covers 12+ logging libraries across Node.js, Go, Python, Rust, and Java ecosystems.

**Level field** (checked in this order):
| Priority | Field Name | Libraries |
|----------|-----------|-----------|
| 1 | `level` | pino, bunyan, zap, zerolog, structlog, tracing-subscriber |
| 2 | `severity` | Google Cloud Logging, GCP-oriented libraries |
| 3 | `levelname` | Python stdlib logging |
| 4 | `lvl` | zerolog (compact mode) |
| 5 | `log_level` | occasional custom usage |
| 6 | `loglevel` | occasional custom usage |
| 7 | `log.level` | ECS (Elastic Common Schema) |
| 8 | `levelno` | Python stdlib (integer -- needs conversion: 10=DEBUG, 20=INFO, 30=WARNING, 40=ERROR, 50=CRITICAL) |

**Message field** (checked in this order):
| Priority | Field Name | Libraries |
|----------|-----------|-----------|
| 1 | `msg` | zap, zerolog, logrus, pino, bunyan |
| 2 | `message` | winston, structlog, SLF4J/Logback, Log4j2 |
| 3 | `event` | structlog (when used as event name) |
| 4 | `text` | occasional custom usage |
| 5 | `body` | occasional custom usage |

**Timestamp field** (checked in this order):
| Priority | Field Name | Libraries |
|----------|-----------|-----------|
| 1 | `timestamp` | structlog, logrus |
| 2 | `time` | zap, zerolog, bunyan |
| 3 | `ts` | zap (short form -- may be epoch float) |
| 4 | `@timestamp` | Elasticsearch/ECS, Logstash |
| 5 | `datetime` | occasional custom usage |
| 6 | `asctime` | Python stdlib logging |
| 7 | `created` | Python stdlib (epoch float) |
| 8 | `timeMillis` | Log4j2 (epoch millis integer) |

### Additional Well-Known Fields

These are not promoted to `LogEntry` struct fields but are extracted into the `fields` HashMap when present. The extractor checks each group in priority order.

| Semantic Field | Candidate Names (priority order) |
|---------------|----------------------------------|
| caller/source | `caller`, `source`, `logger`, `logger_name`, `name` |
| error | `error`, `err`, `exception`, `error.message` |
| stack trace | `stack_trace`, `stacktrace`, `stack`, `error.stack_trace`, `exception.stacktrace` |
| hostname | `hostname`, `host`, `host.name` |
| PID | `pid`, `process`, `process.pid` |
| service | `service`, `service.name`, `app`, `application` |
| trace ID | `trace_id`, `traceId`, `trace.id`, `dd.trace_id` |
| span ID | `span_id`, `spanId`, `span.id`, `dd.span_id` |
| request ID | `request_id`, `requestId`, `req_id`, `x-request-id` |

---

## LogEntry Struct

The universal log record. Everything downstream (filtering, search, display, export) works with this type. Must implement `Clone` (required by `tokio::broadcast`).

```rust
#[derive(Clone, Debug)]
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
- **`Clone` is derived.** Required by `tokio::broadcast::Sender`. All fields (String, Option<String>, HashMap, ParsedContent) are Clone-able. `ParsedContent` must also derive Clone (serde_json::Value is Clone).

### Future: String Storage Optimization

Currently `raw` is an owned `String`. With hundreds of thousands of entries, this means hundreds of thousands of small allocations. A future optimization could store raw output in a contiguous backing buffer and use byte-range references instead of owned strings. Not worth pursuing until it's a measured bottleneck -- noting it here so the API doesn't preclude it.

---

## Filter Expression Engine (Wave 2)

### Syntax: Lucene/Datadog-style `key:value`

Chosen for CLI ergonomics: colon is not a shell metacharacter, so simple queries need zero quoting. Only OR/AND with spaces need quoting.

### Grammar

```
query       = expr (bool_op expr)*
expr        = NOT expr | '(' query ')' | term
bool_op     = AND | OR
term        = '-'? field ':' value | '-'? bare_text
field       = identifier ('.' identifier)*
value       = comparison_value | regex_value | quoted_string | wildcard_string | bare_word
```

Where:
- `comparison_value` = `>`, `>=`, `<`, `<=` followed by a number (e.g., `status:>400`)
- `regex_value` = `/pattern/` (e.g., `message:/connect.*refused/`)
- `quoted_string` = `"..."` for values containing spaces
- `wildcard_string` = bare word containing `*` or `?`
- `bare_text` without a field prefix is a full-text search across `raw` and `message`

### Examples

| Query | Meaning |
|-------|---------|
| `level:error` | Level equals "error" |
| `level:error service:auth` | Implicit AND -- level is error AND service is auth |
| `"level:error OR level:warn"` | Shell-quoted to allow OR |
| `-level:debug` | Negation -- exclude debug level |
| `message:/connect.*refused/` | Regex match on message field |
| `status:>400` | Numeric comparison on status field |
| `service:auth*` | Wildcard match |
| `connection refused` | Full-text search (bare text, no field prefix) |
| `level:error AND (service:auth OR service:api)` | Grouped boolean logic |

### Field Resolution

1. Well-known fields first: `level`, `message`, `timestamp`, `source`, `raw`
2. Then `fields` HashMap lookup
3. Dotted keys (e.g., `error.message`) traverse into nested `serde_json::Value` within the HashMap

### AST Types

```rust
pub enum FilterExpr {
    And(Box<FilterExpr>, Box<FilterExpr>),
    Or(Box<FilterExpr>, Box<FilterExpr>),
    Not(Box<FilterExpr>),
    Term(FilterTerm),
}

pub struct FilterTerm {
    pub negated: bool,
    pub field: Option<FieldPath>,
    pub matcher: Matcher,
}

pub struct FieldPath(pub Vec<String>);  // e.g., ["error", "message"]

pub enum Matcher {
    Exact(String),
    Substring(String),
    Regex(regex::Regex),
    Comparison(CmpOp, f64),
    Wildcard(String),  // compiled to regex internally
}

pub enum CmpOp { Gt, Gte, Lt, Lte }
```

### Parser: winnow v0.7

Chosen over alternatives:
- **chumsky** -- overkill for this grammar size, heavier dependency
- **pest** -- requires separate grammar file, harder to iterate
- **hand-rolled** -- more boilerplate for worse error messages

winnow provides zero extra dependencies (it's self-contained), good error messages via `ContextError`, and is right-sized for this grammar (~200 lines of parser code).

---

## Cmd Extension

The `Cmd` type (from Phase 3b) gains optional log engine configuration:

```rust
// Internal storage on Cmd struct:
// parser: Option<Box<dyn RecordParser>>,
// extractor: Option<Box<dyn FieldExtractor>>,

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

### Clone Handling

Adding `Box<dyn RecordParser>` and `Box<dyn FieldExtractor>` to Cmd breaks `derive(Clone)`. Research confirms `Cmd` is only cloned in tests, never in production code (`exec`/`spawn` take `impl Into<Cmd>` and move). Two options:

- **Option A (recommended):** Remove `derive(Clone)` from Cmd entirely. Fix the few test sites that clone.
- **Option B:** Implement Clone manually, setting parser/extractor to None on clone.

Decision: **Option A** -- removing derive(Clone). It's cleaner and the test impact is minimal.

`From<&str>` and `From<Command>` conversions set parser and extractor to `None` (autodetect defaults).

---

## Integration Plan

### Parser Lifecycle

Each `exec()` / `spawn()` call gets its own parser instance and `BytesMut` buffer. The parser is sourced from `Cmd` or falls back to the default chain.

- `Cmd` carries `Option<Box<dyn RecordParser>>` and `Option<Box<dyn FieldExtractor>>`
- In `exec()` / `spawn()`: extract the parser from Cmd (if present) or construct the default `FallbackParser`
- Stream handler creates a `BytesMut` buffer, reads raw bytes from stdout/stderr, feeds the parser, advances past consumed bytes
- Each call gets a fresh parser instance and buffer -- no sharing across calls
- `BufReader::lines()` is replaced by `read_buf()` into `BytesMut` -- no pre-splitting on newlines

### Field Extractor Lifecycle

One per `exec()` / `spawn()` call. Sourced from `Cmd` or default `LayeredExtractor` with `CommonJsonFieldExtractor`.

### OutputBuffer Migration

`OutputBuffer` changes from `VecDeque<LogLine>` to `VecDeque<LogEntry>` and `broadcast::Sender<LogLine>` to `broadcast::Sender<LogEntry>`.

**Complete use-site inventory:**

| Location | Current Usage | Migration |
|----------|--------------|-----------|
| `process.rs:21-24` | `LogLine` enum definition | Replace with `use log::LogEntry` |
| `process.rs:26-42` | `LogLine` impl | Remove (functionality moves to log module) |
| `process.rs:91-142` | `OutputBuffer` (VecDeque + broadcast) | Change to `LogEntry` |
| `process.rs:273,280` | `exec()` uses `LogLine::from_line()` for stdout | Use parser pipeline to produce `LogEntry` |
| `process.rs:292,299` | `exec()` uses `LogLine::from_line()` for stderr | Use parser pipeline to produce `LogEntry` |
| `process.rs:347,357` | `spawn()` uses `LogLine::from_line()` | Use parser pipeline to produce `LogEntry` |
| `process.rs` tests | ~15 test functions reference LogLine | Update to use `LogEntry` |
| `task.rs:81` | Returns `Vec<LogLine>` | Returns `Vec<LogEntry>` |
| `prelude.rs:3` | Re-exports LogLine, OutputBuffer | Re-export LogEntry, OutputBuffer |
| `signal.rs:108+` | Uses OutputBuffer in tests | No direct LogLine refs -- works after OutputBuffer change |

### spawn() Buffer Isolation

Current behavior: `spawn()` creates a separate buffer not connected to TaskContext (the buffer ref is dropped -- this is an existing bug). The separation is architecturally correct, but `ProcessHandle` should carry `Arc<Mutex<OutputBuffer>>` for access. Composition across sources is a log store concern (wave 2).

### Migration Checklist

1. Create `crates/runme/src/log/` module directory
2. Create `log/mod.rs` -- shared types: `LogEntry`, `ParsedContent`, `RawRecord`, `ParseResult`, `ExtractedFields`
3. Create `log/parse.rs` -- `RecordParser` trait, `FallbackParser`, `JsonlParser`, `RustPanicParser`, `CargoDiagnosticParser`, `LogfmtParser`, `PlainLineParser`
4. Create `log/extract.rs` -- `FieldExtractor` trait, `LayeredExtractor`, `CommonJsonFieldExtractor`
5. Add `mod log;` to `lib.rs`
6. Add `Cmd` fields: `parser: Option<Box<dyn RecordParser>>`, `extractor: Option<Box<dyn FieldExtractor>>`
7. Add `Cmd::record_parser()` and `Cmd::field_extractor()` builder methods
8. Remove `derive(Clone)` from `Cmd`, fix affected test sites
9. Update `From<&str>` and `From<Command>` impls to set parser/extractor to `None`
10. Remove `LogLine` enum and its impl from `process.rs`
11. Update `OutputBuffer` to use `LogEntry` instead of `LogLine`
12. Add `bytes = "1"` dependency to `Cargo.toml`
13. Update `exec()`: replace `BufReader::lines()` with `BytesMut` + `read_buf()` loop, feed bytes to parser, advance buffer on Record
14. Update `spawn()` similarly, ensure `ProcessHandle` carries `Arc<Mutex<OutputBuffer>>`
14. Update `task.rs` return type from `Vec<LogLine>` to `Vec<LogEntry>`
15. Update `prelude.rs` re-exports
16. Migrate all existing tests to use `LogEntry`
17. Add new tests for each parser, extractor, and combinator
18. `cargo test --workspace` passes

---

## Presentation (Future Work)

Multiple traits will be needed for presenting log data. Not designed yet -- waiting until the data model is solid. Known concerns:

- Rendering a single entry as text (terminal, export, JSON lines)
- Rendering a stream/view (headers, separators, color, interleaving markers)
- Summarizing (counts by level, by source -- what agent mode would want)
- Diffing/highlighting (what changed between runs)

The `LogEntry` struct should carry enough information to support all of these. If we discover it doesn't, that's a signal to revisit the struct.

---

## Module Layout

```
crates/runme/src/
+-- log/
|   +-- mod.rs          -- LogEntry, ParsedContent, RawRecord, ParseResult, ExtractedFields
|   +-- parse.rs        -- RecordParser trait, FallbackParser, built-in parsers
|   +-- extract.rs      -- FieldExtractor trait, LayeredExtractor, CommonJsonFieldExtractor
|   +-- filter.rs       -- filter expression engine (wave 2)
|   +-- store.rs        -- log store, multi-source composition (wave 2)
|   +-- search.rs       -- full-text search, context windows (wave 2)
|   +-- stream.rs       -- re-streaming, export (wave 3)
+-- ...existing files...
```

`LogLine` in process.rs is replaced by the new `LogEntry` type. `OutputBuffer` evolves to hold `LogEntry`.

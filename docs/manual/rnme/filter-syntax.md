# rnme log filter syntax

The `filter` parameter on `get_logs` accepts a small expression language for narrowing log entries server-side. Filtering happens engine-side, so the wire only carries matches.

## Common forms

```
level=error
level>=warn
source=t42
field.service=api
field.latency_ms>100
message~"timeout|deadline"
```

## Operators

- `=` exact match
- `!=` not equal
- `>`, `>=`, `<`, `<=` — numeric comparison (parses RHS as number)
- `~` regex match against string
- `!~` negated regex

## Fields you can filter on

- `level` — `trace` / `debug` / `info` / `warn` / `error`
- `source` — task ID (without the `t` prefix)
- `message` — the parsed message string
- `raw` — unparsed line text
- `field.<key>` — any structured field extracted from JSON / logfmt records

## Combinators

Expressions can be combined with `and`, `or`, `not`, and parentheses:

```
level>=warn and field.service=api
not (source=t42 or source=t43)
field.latency_ms>500 and message~"slow"
```

## Examples

```
# All warnings and errors from a task
get_logs(task_id="42", filter="level>=warn")

# Only entries from a specific service field
get_logs(task_id="42", filter="field.service=database")

# Errors mentioning timeout
get_logs(task_id="42", filter="level=error and message~timeout")
```

A `FilterParse` error in the response means the expression didn't parse — check operators and quoting on regex literals.

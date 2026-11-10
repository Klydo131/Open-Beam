# Architecture

This document explains how Open Beam is put together and why. It describes the
code as it exists today and marks planned parts as such.

## Overview

Open Beam is one executable. Inside it, three layers each do one job:

```text
 Excel / Power BI / any OData client
              │  HTTP (OData v4, read-only)
┌─────────────▼──────────────┐
│ beam-server                │  configuration, HTTP, authentication
├────────────────────────────┤
│ beam-odata                 │  protocol: parse request → parameterised SQL
├────────────────────────────┤
│ beam-engine                │  embedded DuckDB, dataset catalog, sandbox
└─────────────┬──────────────┘
              │  read-only
        your files (CSV, Parquet)
```

Dependencies point downward only. `beam-odata` performs no I/O and does not
depend on the engine, so the protocol logic can be tested exhaustively without
a database, and a second protocol can be added beside it without touching it.

## Layers

### `beam-engine`

Owns an in-memory DuckDB database. At start-up it creates one view per
configured dataset, then seals the database (see *Security model*). A
dataset is either a file or a `SELECT` over datasets declared before it.

- **Stable names.** Column headers in real files contain spaces and symbols.
  The engine derives a simple, unique identifier for every column, so the
  layers above never handle arbitrary names.
- **A small type system.** Every source type is mapped to one of eight exposed
  types (boolean, 32- and 64-bit integer, double, decimal, text, date,
  timestamp). Types with no equivalent are served as text rather than dropped.
- **Keys.** Tabular clients need a way to identify a row. A dataset either
  names a key column or receives a `_row` number column.

### `beam-odata`

Implements the subset of [OData v4](https://www.odata.org/documentation/) that
spreadsheet and BI clients use to read tables: the service document,
`$metadata`, and collections with `$select`, `$filter`, `$orderby`, `$top`,
`$skip` and `$count`.

A request is planned in two steps. The `$filter` text is parsed into an
expression tree by a hand-written recursive-descent parser with explicit limits
on length and nesting. The tree is then rendered to SQL in which every
client-supplied value is a bind parameter and every name has been checked
against the dataset's schema.

### `beam-server`

Loads `beam.toml`, opens the engine, and serves HTTP with
[axum](https://github.com/tokio-rs/axum). Queries run on a blocking thread pool
so a slow query does not stall other requests. Large results are returned in
pages; each response includes a link to the next page, which OData clients
follow automatically.

## Security model

Open Beam assumes the person who writes `beam.toml` is trusted and that
everything arriving over the network is not.

| Threat | Defence |
| --- | --- |
| SQL injection through query options | Values are bound as parameters; identifiers are validated against the schema; unsupported syntax is rejected by the parser. |
| Reading files that were not shared | After start-up DuckDB's file access is restricted to the configured dataset files, external access is disabled and the configuration is locked. |
| Modifying or deleting data | The database is in-memory, datasets are views, and the service only issues `SELECT` statements. |
| Unintended network exposure | The default bind address is loopback. Binding elsewhere without authentication is refused at start-up. |
| Resource exhaustion | Filters are bounded in size and depth; responses are bounded by `max_page_size`. |
| Information leaks in errors | Engine error text is logged on the server and never returned to the client. |
| Unsafe Rust | `unsafe` is forbidden in every crate in this workspace. |

Known limits are listed in [SECURITY.md](../SECURITY.md).

## Planned

These are design intentions, not implemented features. Order and scope are
tracked in the [roadmap](ROADMAP.md).

- **PostgreSQL wire protocol.** A second listener that lets any
  PostgreSQL-capable client send SQL. It will sit beside `beam-odata` and share
  `beam-engine`.
- **Database sources.** Datasets backed by PostgreSQL and SQLite databases that
  the user operates. Open Beam will hold connection settings only.
- **Web interface.** A browser UI served by the same binary: dataset browser,
  query editor, and dashboards built from panels.
- **Assistant integrations.** An optional provider interface for language
  models, disabled by default. Users supply their own credentials, which stay
  on their machine. Applications built on Open Beam choose and are responsible
  for their own integrations.
- **Sandboxed extensions.** Third-party extensions compiled to WebAssembly and
  run with explicit, minimal permissions.

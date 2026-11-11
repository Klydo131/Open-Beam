# Architecture

This document explains how Open Beam is put together and why. It describes the
code as it exists today and marks planned parts as such.

## Overview

Open Beam is one executable. Inside it, four crates each do one job:

```text
   Browser                 Excel / Power BI / any OData client
      │ HTML pages                │ OData v4, read-only
┌─────▼───────────────────────────▼─────┐
│ beam-server                           │  configuration, pages, sign-in
├──────────────────┬────────────────────┤
│ beam-accounts    │ beam-odata         │  who may ask  │  what is asked
│ (SQLite file)    ├────────────────────┤
│                  │ beam-engine        │  embedded DuckDB, sealed
└──────────────────┴─────────┬──────────┘
                             │ read-only
                    your files (CSV, Parquet)
```

Dependencies point downward only. `beam-odata` performs no I/O and does not
depend on the engine, and `beam-accounts` knows nothing about HTTP, so each can
be tested on its own and reused by another application.

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

### `beam-accounts`

Stores accounts and browser sessions in one SQLite file owned by whoever runs
Open Beam.

- **Passwords** are stored as Argon2id hashes. Checking a password for an
  unknown username takes as long as for a known one.
- **Sessions** are 256-bit random tokens. The browser holds the token; the
  file holds only its SHA-256 digest, so a copy of the file signs nobody in.
- **Invariants live here, not in the pages.** The rule that at least one
  active administrator always exists is enforced inside a transaction, and
  creating the first administrator is a single statement that cannot succeed
  twice.

### `beam-server`

Loads `beam.toml`, opens the engine and the account store, and serves HTTP
with [axum](https://github.com/tokio-rs/axum).

- **Pages** are rendered on the server with
  [maud](https://maud.lambda.xyz), which escapes every value it interpolates.
  They contain no JavaScript. Sorting, filtering and paging are ordinary links
  and forms, and the dataset browser reuses the same query planner as the
  feed.
- **The feed** accepts a browser session or HTTP Basic credentials for an
  account. Because password checks are deliberately slow and feed clients send
  credentials with every request, a successful check is remembered for a few
  minutes, keyed by a digest, and forgotten whenever any account changes.
- **Charts** are SVG drawn on the server from a grouped summary, styled by
  the page's stylesheet, with the same figures repeated in a table for anyone
  who cannot see the drawing.
- **Programs** sign in with personal API tokens: long random secrets of
  which only a digest is stored. They reach a read-only REST API under
  `/api/v1`, described by a hand-written OpenAPI document, and a read-only
  MCP endpoint at `/mcp` for AI assistants. Both are thin layers over the
  same planner as the feed and see exactly what the feed would.
- **PostgreSQL access** (off unless `postgres_bind` is set) speaks the
  PostgreSQL wire protocol directly. Each statement is parsed by DuckDB's own
  parser into a syntax tree, and the tree is checked against short lists of
  allowed query shapes, table references and functions before it is turned
  back into SQL and run. Because the checked tree is what runs, the check and
  the engine cannot disagree about what a statement means. Catalog tables
  such as `information_schema.tables` are rebuilt for each account from the
  datasets it can see.
- Queries and password hashing run on a blocking thread pool so they do not
  stall other requests, and every query is cancelled when it outlasts the
  configured time limit.

## Security model

Open Beam assumes the person who writes `beam.toml` is trusted and that
everything arriving over the network is not.

| Threat | Defence |
| --- | --- |
| SQL injection through the feed or the filter form | Values are bound as parameters; identifiers are validated against the schema; unsupported syntax is rejected by the parser. |
| Reading files that were not shared | After start-up DuckDB's file access is restricted to the configured dataset files, external access is disabled and the configuration is locked. |
| Modifying or deleting data | The database is in-memory, datasets are views, and only `SELECT` statements are issued. |
| Unintended network exposure | The default bind address is loopback. Open Beam refuses to listen elsewhere, or behind a public address, until an administrator exists. |
| Password guessing | Argon2id hashing; five failures lock a username for five minutes; one message for every failed sign-in. |
| Stolen account file | It contains password hashes and session digests, not passwords or tokens. |
| Forged requests from other sites | Session cookies are `SameSite=Strict` and `HttpOnly`; every form carries a per-session token; posts that the browser labels as cross-site are refused. |
| Script injection through data | Pages escape all values, contain no scripts, and are served with a content policy that allows none. |
| Clickjacking | Pages may not be framed. |
| Privilege escalation | Role checks run on every administrative request; the last active administrator cannot be demoted or disabled. |
| Stale access | Disabling an account or changing its password ends its sessions and its feed access at once. |
| Resource exhaustion | Filters are bounded in size and depth; responses are bounded by a page size; every query has a time limit; passwords are bounded in length before hashing; sign-in checks are bounded in number and in queue length. |
| Arbitrary SQL from PostgreSQL clients | Only `SELECT`; the engine's own syntax tree is checked against allow-lists of query shapes, tables and functions; datasets the account cannot see look like missing tables; account passwords are never accepted, only revocable tokens. |
| Information leaks in errors | Engine error text is logged on the server and not returned to the client. The one exception is PostgreSQL access, which repeats the engine's explanation of a mistake in the client's own query (an unknown column, a value of the wrong type) only when it names no file. |
| Unsafe Rust | `unsafe` is forbidden in every crate in this workspace. |

Known limits are listed in [SECURITY.md](../SECURITY.md).

## Planned

These are design intentions, not implemented features. Order and scope are
tracked in the [roadmap](ROADMAP.md).

- **PostgreSQL catalog compatibility.** Enough of PostgreSQL's internal
  catalog for the drivers behind Power BI's and Excel's PostgreSQL
  connectors.
- **Database sources.** Datasets backed by PostgreSQL and SQLite databases that
  the user operates. Open Beam will hold connection settings only.
- **Dashboards.** Several charts arranged as panels on one page, in the style
  of monitoring tools.
- **Assistant integrations.** Beyond the read-only MCP endpoint, an optional provider interface for language
  models, disabled by default. Users supply their own credentials, which stay
  on their machine. Applications built on Open Beam choose and are responsible
  for their own integrations.
- **Sandboxed extensions.** Third-party extensions compiled to WebAssembly and
  run with explicit, minimal permissions.

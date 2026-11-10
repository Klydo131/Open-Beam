# Roadmap

Milestones are ordered. Each one should leave the project usable on its own.

## 0.1 — Data feed for Excel and Power BI

- [x] Rust workspace with separate engine, protocol and server crates
- [x] CSV and Parquet datasets declared in `beam.toml`
- [x] Query datasets: summarise, join and de-duplicate with SQL
- [x] Read-only OData v4 feed: service document, `$metadata`, `$select`,
      `$filter`, `$orderby`, `$top`, `$skip`, `$count`, server-driven paging
- [x] Sealed engine: file access limited to configured datasets
- [x] Optional HTTP Basic authentication; refused start-up when exposed without it
- [x] Continuous integration on Linux, Windows and macOS
- [ ] Interoperability testing with current Excel and Power BI Desktop releases
- [ ] Pre-built binaries for Windows, macOS and Linux

## 0.2 — Connect to your databases

- [ ] PostgreSQL and SQLite as dataset sources
- [ ] Excel workbooks (`.xlsx`) as dataset sources
- [ ] Reload datasets without restarting
- [ ] Built-in TLS

## 0.3 — PostgreSQL wire protocol

- [ ] Read-only SQL access for PostgreSQL-compatible clients
- [ ] Catalog compatibility for the drivers used by Power BI and Excel
- [ ] Per-query time and memory limits

## 0.4 — Web interface

- [ ] Dataset browser and SQL editor in the browser
- [ ] Charts and dashboards built from panels
- [ ] Saved queries and dashboards stored in a local SQLite file

## 0.5 — Platform

- [ ] Users, roles and per-dataset permissions
- [ ] Optional assistant integrations using the user's own credentials
- [ ] Sandboxed WebAssembly extensions
- [ ] Stable, versioned Rust and HTTP APIs

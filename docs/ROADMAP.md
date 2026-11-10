# Roadmap

Milestones are ordered. Each one should leave the project usable on its own.

## 0.1 — Data feed for Excel and Power BI

- [x] Rust workspace with separate engine, protocol and server crates
- [x] CSV and Parquet datasets declared in `beam.toml`
- [x] Query datasets: summarise, join and de-duplicate with SQL
- [x] Read-only OData v4 feed: service document, `$metadata`, `$select`,
      `$filter`, `$orderby`, `$top`, `$skip`, `$count`, server-driven paging
- [x] Sealed engine: file access limited to configured datasets
- [x] Continuous integration on Linux, Windows and macOS

## 0.2 — Web interface and accounts

- [x] Accounts stored in a local SQLite file, with Argon2id password hashing
- [x] Two roles: administrator and user
- [x] First-run set-up, sign-in, sign-out, change password
- [x] Account management: add, change role, disable, reset password
- [x] Dataset browser: sort, filter and page through tables
- [x] Feed sign-in with account credentials
- [x] Sign-in attempt limiting, anti-forgery tokens, strict content policy

## 0.3 — Your own data, together

- [x] Add CSV, Excel (.xlsx) and Parquet files from the browser
- [x] Uploaded datasets are private to their owner until shared
- [x] People directory and personal contacts
- [x] Share and stop sharing dataset by dataset; delete your own datasets
- [x] Download the current view as Excel or CSV; Google Sheets import guidance
- [x] Basic and Advanced modes in Settings; profile name and team
- [x] Advanced: column choice, more filters, feed address and Power Query for a view
- [x] Starts with no configuration file and opens the browser
- [x] Release workflow building downloads for Windows, macOS and Linux
- [ ] First published release, and signed downloads
- [ ] Combine datasets from the browser (join, de-duplicate) without SQL

## 0.4 — Connect to your data

- [ ] PostgreSQL and SQLite as dataset sources
- [ ] Excel workbooks (`.xlsx`) as dataset sources
- [ ] Reload datasets without restarting
- [ ] Pre-built binaries for Windows, macOS and Linux
- [ ] Built-in TLS

## 0.5 — Dashboards and automation

- [x] Charts on dataset pages: bars and lines, grouped by column or period
- [ ] Dashboards built from panels
- [ ] Saved views
- [ ] Scheduled refresh and other automation
- [ ] Export to CSV and Excel

## 0.6 — PostgreSQL wire protocol

- [ ] Read-only SQL access for PostgreSQL-compatible clients
- [ ] Catalog compatibility for the drivers used by Power BI and Excel
- [ ] Per-query time and memory limits

## 0.7 — Platform

- [ ] Review and sign-off workflow for reported figures
- [ ] Audit log of account and data-access events
- [ ] Optional assistant integrations using the user's own credentials
- [ ] Sandboxed WebAssembly extensions
- [ ] Stable, versioned Rust and HTTP APIs

<p align="center">
  <img src="assets/banner.svg" alt="Open Beam: open-source analytics in Rust. Your data, in the tools you already use." width="100%">
</p>

# Open Beam

Open Beam is an open-source analytics platform written in Rust. It runs as a
single program on your own computer or server, reads the data you point it at,
lets your team explore it in the browser, and makes it available to the tools
you already use, starting with Microsoft Excel and Power BI.

> **Status: early release (0.3).** Adding your own files, sharing, charts,
> accounts, and export to Excel, CSV, Google Sheets and Power BI work and are
> covered by automated tests. Dashboards, database connections and automation
> are still to come. See the [roadmap](docs/ROADMAP.md).

<p align="center">
  <img src="docs/images/screens/table.png" alt="A dataset in the Open Beam web interface, filtered to one region and sorted by units." width="100%">
</p>

<p align="center">
  <img src="docs/images/screens/chart.png" alt="A bar chart of units sold by product in the Open Beam web interface." width="100%">
</p>

## What you get

- **Bring your own data, checked first.** Add CSV, Excel or Parquet files from
  the browser. Each file waits as a private draft while you review it: Open
  Beam points out empty rows and rows reported twice, you choose what to tidy,
  and only then is it saved.
- **A simple web interface.** Browse, sort, filter and page through tables.
  **Basic** mode keeps it simple; **Advanced** adds column choice, more
  filters and ready-made Excel and Power BI connections. No scripts run in
  the browser.
- **Work together.** A People directory with personal contacts, and sharing
  dataset by dataset.
- **Export anywhere.** Download exactly the view on screen as Excel or CSV
  (for Google Sheets), or connect Power BI and Excel live.
- **Charts.** Summarise any dataset as bars or a line: counts, sums and
  averages, grouped by a column or by day, month, quarter or year.
- **Accounts with two roles.** Administrators manage accounts; users add,
  explore and share their own data. See [Manage accounts](docs/guides/05-manage-accounts.md).
- **A data feed for Excel and Power BI.** Load any dataset with the built-in
  OData connector. No driver to install.
- **Clean data in one place.** Define summaries, joins and de-duplicated
  tables once, in SQL, and every report built on them agrees.

## Principles

- **Fast.** Queries run on an embedded columnar engine (DuckDB) inside a
  compiled Rust binary. There is no separate database server to install.
- **Secure by default.** Open Beam listens only on your own machine unless you
  configure otherwise, requires sign-in, reads only the files you list, and
  never modifies them. See [SECURITY.md](SECURITY.md).
- **Your data stays yours.** No telemetry and no hosted service. Data lives
  wherever you keep it, and accounts live in a file on your machine.
- **Educational.** The codebase is small, layered and documented so it can be
  read as a worked example of building a data service in Rust. Start with
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
- **Extensible.** The engine, protocol and account layers are separate Rust
  crates that other applications can build on.

## How it works

<p align="center">
  <img src="docs/images/how-it-works.svg" alt="List your files, start Open Beam and sign in, then explore in your browser or load into Excel or Power BI." width="100%">
</p>

## Use it (no technical skills needed)

Download Open Beam for your computer from the
[Releases page](https://github.com/Klydo131/Open-Beam/releases), unzip it, and
double-click **beam**. Your browser opens; choose a password and select
**Add data**. Full steps: [Start here](docs/guides/00-start-here.md).

> No release has been published yet. Until then, use the developer steps
> below.

## Build it (developers)

You need [rustup](https://rustup.rs), which installs the right Rust version
for you, and a C++ compiler. Full instructions for Windows, macOS and Linux are in
[Install Open Beam](docs/guides/01-install.md).

1. Get the code and build it. The first build compiles the DuckDB engine and
   can take 15 minutes or more; later builds take seconds.

   ```bash
   git clone https://github.com/Klydo131/Open-Beam.git
   cd Open-Beam
   cargo build --release
   ```

2. Copy the example configuration. It points at the sample data in
   `examples/`.

   ```bash
   cp beam.example.toml beam.toml
   ```

3. Start Open Beam.

   ```bash
   cargo run --release -- --config beam.toml
   ```

   ```text
   Open Beam 0.2.0 serving 2 dataset(s)
     Open in your browser:        http://127.0.0.1:8787/
     Data feed (Excel, Power BI): http://127.0.0.1:8787/odata/
     No accounts yet. Open the address above to create the administrator.
   ```

4. Open the address in your browser and create the administrator account.

5. To use the data elsewhere, connect a tool to the feed address and sign in
   with **Basic** using your Open Beam username and password.
   - **Excel:** Data → Get Data → From Other Sources → From OData Feed.
   - **Power BI Desktop:** Get Data → OData feed.

## Guides

Step-by-step guides, in order:

0. [Start here](docs/guides/00-start-here.md) — for everyone
1. [Install Open Beam](docs/guides/01-install.md) — build from source
2. [Start Open Beam and see your data](docs/guides/02-first-dataset.md)
3. [Connect Excel and Power BI](docs/guides/03-connect-excel-and-power-bi.md)
4. [Combine and clean data](docs/guides/04-combine-and-clean-data.md)
5. [Manage accounts](docs/guides/05-manage-accounts.md)
6. [Run Open Beam for a team](docs/guides/06-run-for-a-team.md)
7. [Add, share and export data](docs/guides/07-add-share-and-export.md)

## Architecture

<p align="center">
  <img src="docs/images/architecture.svg" alt="Your browser, Excel and Power BI read from Open Beam, which reads your files without changing them." width="80%">
</p>

Four parts, each with one job. The design and the security model are
explained in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Project layout

| Path | Purpose |
| --- | --- |
| `crates/beam-engine` | Embedded query engine: dataset catalog, type mapping, sandboxed execution |
| `crates/beam-odata` | OData v4 protocol: query parsing, SQL planning, metadata |
| `crates/beam-accounts` | Accounts, roles and sessions in a local SQLite file |
| `crates/beam-server` | The `beam` binary: configuration, web interface, data feed |
| `docs/` | Architecture, roadmap and user guides |
| `examples/` | Sample data and a worked reconciliation example |
| `assets/` | Logo and banner |

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md)
first, and report security issues privately as described in
[SECURITY.md](SECURITY.md).

## License

Open Beam is released under the [MIT License](LICENSE).

Open Beam is an independent project. It is not affiliated with or endorsed by
the Apache Software Foundation or the Apache Beam project, nor by Microsoft.
Excel and Power BI are trademarks of the Microsoft group of companies.

<p align="center">
  <img src="assets/banner.svg" alt="Open Beam: open-source analytics in Rust. Your data, in the tools you already use." width="100%">
</p>

# Open Beam

Open Beam is an open-source analytics platform written in Rust. It runs as a
single program on your own computer or server, reads the data you point it at,
and makes that data available to the tools you already use, starting with
Microsoft Excel and Power BI.

> **Status: early prototype (0.1).** The first milestone, a read-only data feed
> for Excel and Power BI, is implemented and covered by automated tests. It has
> not yet been validated against every Excel and Power BI release. See the
> [roadmap](docs/ROADMAP.md) for what is done and what is planned.

## Principles

- **Fast.** Queries run on an embedded columnar engine (DuckDB) inside a
  compiled Rust binary. There is no separate database server to install.
- **Secure by default.** Open Beam listens only on your own machine unless you
  configure otherwise, reads only the files you list, and never modifies them.
  See [SECURITY.md](SECURITY.md).
- **Your data stays yours.** Open Beam ships with no bundled database, no
  telemetry and no hosted service. Data lives wherever you keep it.
- **Educational.** The codebase is small, layered and documented so it can be
  read as a worked example of building a data service in Rust. Start with
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
- **Extensible.** The engine and protocol layers are separate Rust crates that
  other applications can build on.

## How it works

<p align="center">
  <img src="docs/images/how-it-works.svg" alt="List your files, start Open Beam, then load and refresh in Excel or Power BI." width="100%">
</p>

## Quick start

You need a [Rust toolchain](https://rustup.rs) (1.88 or newer) and a C++
compiler. Full instructions for Windows, macOS and Linux are in
[Install Open Beam](docs/guides/01-install.md).

1. Get the code and build it. The first build compiles the DuckDB engine and
   can take 15 minutes or more; later builds take seconds.

   ```bash
   git clone https://github.com/Klydo131/Open-Beam.git
   cd Open-Beam
   cargo build --release
   ```

2. Save a CSV file as `data/sales.csv` and copy the example configuration,
   which already points at it.

   ```bash
   cp beam.example.toml beam.toml
   ```

3. Start Open Beam.

   ```bash
   cargo run --release -- --config beam.toml
   ```

   ```text
   Open Beam 0.1.0 serving 1 dataset(s)
     OData feed: http://127.0.0.1:8787/odata/
     Sign-in:    none (local only)
   ```

4. Connect a tool to the feed address.
   - **Excel:** Data → Get Data → From Other Sources → From OData Feed.
   - **Power BI Desktop:** Get Data → OData feed.

## Guides

Step-by-step guides, in order:

1. [Install Open Beam](docs/guides/01-install.md)
2. [Share your first dataset](docs/guides/02-first-dataset.md)
3. [Connect Excel and Power BI](docs/guides/03-connect-excel-and-power-bi.md)
4. [Combine and clean data](docs/guides/04-combine-and-clean-data.md)
5. [Protect a feed with a password](docs/guides/05-protect-a-feed.md)
6. [Run Open Beam for a team](docs/guides/06-run-for-a-team.md)

## Architecture

<p align="center">
  <img src="docs/images/architecture.svg" alt="Excel, Power BI and other tools read an OData feed from Open Beam, which reads your files without changing them." width="80%">
</p>

Three layers, each with one job. The design and the security model are
explained in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Project layout

| Path | Purpose |
| --- | --- |
| `crates/beam-engine` | Embedded query engine: dataset catalog, type mapping, sandboxed execution |
| `crates/beam-odata` | OData v4 protocol: query parsing, SQL planning, metadata |
| `crates/beam-server` | The `beam` binary: configuration, HTTP server, authentication |
| `docs/` | Architecture, roadmap and user guides |
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

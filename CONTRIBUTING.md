# Contributing to Open Beam

Thank you for helping. This guide covers how to build, test and propose
changes.

## Building and testing

You need a [Rust toolchain](https://rustup.rs) (1.88 or newer) and a C++
compiler. The first build compiles DuckDB and takes several minutes.

```bash
cargo test --workspace
```

Before opening a pull request, run the checks CI runs:

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

## Ground rules

- **Keep changes small and focused.** One concern per pull request.
- **Write the least code that solves the problem.** Prefer the standard
  library and existing dependencies to new ones. Every new dependency needs a
  reason and an MIT-compatible licence.
- **Test behaviour, especially refusals.** Anything that parses input or
  touches the security model needs a test showing what is rejected as well as
  what is accepted.
- **No `unsafe`.** It is forbidden workspace-wide.
- **Respect the layering** described in
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): protocol crates do no I/O and
  the engine knows nothing about protocols.
- **Never commit data or secrets.** No real datasets, credentials, connection
  strings or personal information, in code, tests, fixtures or commit
  messages. Use invented sample data.

## Commit messages

Use the imperative mood and say what changed and why, for example
`Reject $filter expressions nested deeper than 32 levels`.

## AI-assisted development

Using AI coding tools is welcome. You are responsible for every line you
submit: read it, understand it and test it. Do not include prompts,
transcripts or tool session links in commits, code comments or pull requests.

Maintainers have found these open-source agent plugins useful for keeping
changes small and reviews focused. They are optional.

| Plugin | Purpose | Licence |
| --- | --- | --- |
| [pstack](https://github.com/cursor/plugins/tree/main/pstack) | Engineering playbooks for planning, testing and review | MIT |
| [ponytail](https://github.com/DietrichGebert/ponytail) | Steers agents toward the minimum necessary code | MIT |
| [caveman](https://github.com/JuliusBrussee/caveman) | Terse agent responses to reduce token use | MIT |

## Reporting security issues

See [SECURITY.md](SECURITY.md). Please do not use public issues.

## Licence

By contributing you agree that your contributions are licensed under the
[MIT License](LICENSE).

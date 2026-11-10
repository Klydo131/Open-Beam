# Security Policy

## Reporting a vulnerability

Please do not open a public issue for security problems. Use GitHub's
[private vulnerability reporting](https://github.com/Klydo131/Open-Beam/security/advisories/new)
so the report reaches the maintainers privately. Include the version or commit,
what you observed, and steps to reproduce it. You can expect an acknowledgement
within seven days.

## Supported versions

Open Beam is pre-1.0. Security fixes are applied to the `main` branch only.

## What Open Beam does to protect you

The threat model and defences are described in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#security-model). In short: the
service is read-only, listens only on your own machine by default, reads only
the files you configure, and treats every network request as untrusted.

## Known limits

- **No built-in TLS yet.** To serve Open Beam beyond your own machine, place it
  behind an HTTPS reverse proxy. Basic authentication over plain HTTP exposes
  the password to anyone who can observe the network.
- **One shared credential.** There are no per-user accounts or per-dataset
  permissions yet. Anyone with the password can read every configured dataset.
- **No query time limit yet.** An authenticated client can issue filters that
  are expensive to evaluate on very large files.
- **The configuration file is trusted.** Anyone who can edit `beam.toml` can
  choose which files are served.

## Your data and credentials

Open Beam ships with no database, sample credentials or hosted service, and
sends no telemetry. Do not commit `beam.toml`, data files or passwords to a
public repository; `beam.toml` is ignored by Git in this project for that
reason.

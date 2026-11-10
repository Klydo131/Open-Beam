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
service is read-only, listens only on your own machine by default, requires
sign-in, reads only the files you configure, and treats every network request
as untrusted.

## Known limits

- **No built-in TLS yet.** To serve Open Beam beyond your own machine, place it
  behind an HTTPS reverse proxy and set `public_url` to the HTTPS address, so
  that passwords and session cookies are never sent in the clear.
- **Every account can read every dataset.** Per-dataset permissions are
  planned. Do not configure a dataset that some account holders must not see.
- **Feed clients send the password with each request.** This is how HTTP Basic
  authentication works; it is the method Excel and Power BI support for OData.
- **No query time limit yet.** A signed-in client can issue filters that are
  expensive to evaluate on very large files.
- **The configuration file and the machine are trusted.** Anyone who can edit
  `beam.toml` chooses which files are served, and anyone who can run commands
  on the machine can create an administrator.
- **Sign-in limiting is per username.** Someone who can reach the sign-in page
  can keep a known username locked by guessing repeatedly. Limit who can reach
  the page, and note that the lock clears by itself after five minutes.
- **No audit log yet.** Account changes and sign-ins are not recorded beyond
  each account's last sign-in time.

## Your data and credentials

Open Beam ships with no accounts, no default password and no hosted service,
and sends no telemetry. Accounts are created by you on first run and stored in
`beam-accounts.sqlite`, which holds password hashes, not passwords. Do not
commit `beam.toml`, the accounts file, data files or passwords to a public
repository; this project's `.gitignore` excludes them for that reason.

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

Against attacks that need no account:

- **Slow and flooding clients.** A client must send a request's headers
  within 10 seconds, an idle connection is closed after 10 seconds, and a
  request must be answered within 10 minutes. At most 1,024 connections are
  open at once and 64 from any one address; more are closed at once.
- **Password guessing.** Five failures lock a username for five minutes;
  30 failures from one address, across any usernames, lock that address,
  so one common password cannot be tried against every account. New
  passwords that guessing tools try first (common passwords with digits
  added, keyboard runs, one repeated character) are refused.
- **Other websites.** A page on another site cannot post forms here, load
  our data as a script or image, frame our pages, or time our answers to
  learn about a visitor: browsers label such requests and they are refused.
  Following a link from another site still works. Pages share no window
  with other sites (`Cross-Origin-Opener-Policy`), nothing here may be
  embedded elsewhere (`Cross-Origin-Resource-Policy`), and no page may ask
  for the camera, microphone, location or other devices.
- **Crawlers and AI bots.** `robots.txt` and an `X-Robots-Tag` header ask
  every crawler, search engine and AI training bot to keep nothing; those
  that ignore the request still meet the sign-in.
- **HTTPS.** When `public_url` is an `https://` address, browsers are told
  to use only HTTPS for a year (HSTS) and to upgrade any plain link.
- **Dependencies.** Every dependency is checked against the RustSec
  advisory database on each change and weekly, and updates are proposed
  weekly. Reports can also be sent as described in
  `/.well-known/security.txt` (RFC 9116).

## Known limits

- **Behind a reverse proxy, limits per address see the proxy.** Every
  visitor then arrives from the proxy's address, so the per-address
  connection and sign-in limits do not apply to it (the per-username and
  overall limits still do). Rate-limit by visitor address in the proxy.

- **No built-in TLS yet.** To serve Open Beam beyond your own machine, place it
  behind an HTTPS reverse proxy and set `public_url` to the HTTPS address, so
  that passwords and session cookies are never sent in the clear. Setting
  `public_url` is also what lets sign-in and other forms work when the proxy
  passes on a different host name than the one in the browser.
- **Uploaded files are parsed on the server.** CSV and Parquet are read by
  DuckDB and Excel workbooks by `calamine`. Uploads are limited in size, and
  workbooks that declare more than 256 MB unpacked are refused, but a declared
  size can be falsified, so a crafted workbook can still use a lot of memory.
  Only signed-in accounts can upload.
- **Dataset names are shared across all accounts.** Two datasets cannot
  have the same name, so when a name is refused as "already in use", that
  tells you some dataset by that name exists, even one you cannot see. Avoid
  putting sensitive words in dataset names.
- **Sign-in attempts are limited per username, per address and in total.**
  Five failures lock a username for five minutes, 30 lock an address,
  attempts in flight count towards that,
  an attempt abandoned part-way counts as a failure, at most four password
  checks run at once, and when 32 more are already waiting the server answers
  that it is busy instead of queueing.
- **Built-in datasets are visible to every account.** Datasets from
  `beam.toml` are shared with everyone; datasets added in the browser are
  private to their owner, the people they share with, and administrators.
- **Feed clients send the password with each request.** This is how HTTP Basic
  authentication works; it is the method Excel and Power BI support for OData.
- **Queries are limited in time and memory.** Every query is cancelled after
  `query_timeout_seconds` (30 by default). The engine may use
  `memory_limit_mb` (half the computer's memory by default); a query needing
  more spills to temporary files in the upload folder, capped by
  `spill_limit_mb`, and past that stops with a clear message. At most
  `max_concurrent_queries` run at once. Some engine operations cannot spill,
  so a single very large query can still fail rather than finish.
- **Fast copies of datasets live in the upload folder.** Each file dataset
  is kept as a compressed column-store copy in `.cache` inside the upload
  folder; it holds the same data as the original, so protect that folder as
  you protect the data files.
- **Metrics are for administrators.** `/admin/system` and `/metrics` need an
  administrator. The slow-query log keeps statements with quoted values
  replaced, but table and column names remain visible.
- **PostgreSQL access is unencrypted.** It is off by default. When turned on
  with `postgres_bind`, it accepts only personal API tokens, never account
  passwords, but the token travels in the clear. Keep it on `127.0.0.1` or
  reach it through an encrypted tunnel.
- **API tokens are bearer credentials.** Anyone holding a token can read what
  its account can read until it is revoked, the account is disabled, or the
  account is removed. Open Beam stores only a digest of each token.
- **AI assistants read data you let them read.** The MCP endpoint is
  read-only and tells the assistant to treat dataset contents as data, but an
  assistant may still be misled by text inside a dataset. Connect assistants
  only to datasets you are willing to show them.
- **Watches run on the server's behalf.** A watch is set up by the
  dataset's owner or an administrator and its results, including totals, are
  shown to everyone who can see the dataset. Results are recorded only while
  the dataset exists.
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

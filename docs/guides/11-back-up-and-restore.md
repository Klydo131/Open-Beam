# Back up and restore

Open Beam keeps a few things that only exist on the server: the accounts,
the descriptions, verified questions and watches people wrote, who shares
what, the model file, and the files people added in the browser. One
command copies them all to a folder; another puts them back.

Your own data files named in `beam.toml` are not copied: they are yours
already, and Open Beam never changes them. Neither are the fast copies it
keeps in the upload folder, which it makes again by itself.

## Back up

```bash
beam backup /backups/beam-2026-11-25
```

The folder must be new or empty. The server can keep running: the account
database is copied with SQLite's `VACUUM INTO`, which takes a consistent
copy of a database in use and leaves out rows that were deleted.

The folder then holds:

- `accounts.sqlite`, the account database;
- `model.toml`, when the server has a model file;
- `uploads/`, the files people added;
- `SHA256SUMS`, written last, listing every file with its digest.

A folder without `SHA256SUMS` is an unfinished backup. To check a backup
at any time, on any computer:

```bash
cd /backups/beam-2026-11-25
sha256sum -c SHA256SUMS
```

The account database holds password hashes, so keep backups as private as
the server itself; Open Beam makes the folder readable by you alone.

## Restore

Stop Open Beam first, then:

```bash
beam restore /backups/beam-2026-11-25
```

Before changing anything, Open Beam checks every file against its digest
and checks that the account database is whole. It refuses when anything
has changed, when the list names a file a backup never holds or a file
that is a link, and while Open Beam is running with the same account
database, whatever address or configuration file it was started with.

Then it makes every part ready beside where it goes and puts them in
place together: if one cannot be, those already done are put back, so a
restore happens whole or not at all. Whatever it replaces is kept beside
it, named `… .before-restore-<number>`, so a restore can itself be undone
by moving those back. Remove them once you are sure.

Restoring brings back everything as it was when the backup was made:
accounts created since are gone, and passwords changed since are back to
what they were. Everyone is signed out and signs in again. API tokens are
as they were in the backup, so a token revoked since works again: after
restoring because of a security problem, change passwords and revoke
tokens again.

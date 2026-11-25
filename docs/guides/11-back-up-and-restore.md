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
the server itself.

## Restore

Stop Open Beam first, then:

```bash
beam restore /backups/beam-2026-11-25
```

Before changing anything, Open Beam checks every file against its digest
and checks that the account database is whole. It refuses when anything
has changed, when the list names a file a backup never holds, or when
something is still listening on the server's address.

Then it puts the account database, the model and the uploaded files in
place. Whatever it replaces is kept beside it, named
`… .before-restore-<number>`, so a restore can itself be undone by moving
those back. Remove them once you are sure.

Restoring brings back everything as it was when the backup was made:
accounts created since are gone, and passwords changed since are back to
what they were.

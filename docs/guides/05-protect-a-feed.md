# 5. Protect a feed with a password

By default Open Beam answers only programs on the same computer and asks for
no password. Add one before letting anything else reach it.

## Step 1: Name the user

Add to `beam.toml`:

```toml
[auth]
username = "analyst"
```

## Step 2: Set the password

The password is read from the `BEAM_PASSWORD` environment variable so that it
never sits in a file. Choose a long one.

macOS and Linux:

```bash
export BEAM_PASSWORD='choose-a-long-password'
```

Windows PowerShell:

```powershell
$env:BEAM_PASSWORD = 'choose-a-long-password'
```

## Step 3: Restart and check

Start Open Beam from the same terminal. It should print:

```text
  Sign-in:    Basic (username and password)
```

Opening the feed in a browser now asks for the username and password. In
Excel or Power BI choose **Basic** when asked how to sign in.

## What this does and does not protect

- Everyone who knows the password can read every dataset. Per-user accounts
  are planned.
- Over plain `http://` the password travels unencrypted. On a network, always
  put Open Beam behind HTTPS. See [Run Open Beam for a team](06-run-for-a-team.md).

Next: [Run Open Beam for a team](06-run-for-a-team.md).

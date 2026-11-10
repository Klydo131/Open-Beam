# 1. Install Open Beam

Open Beam is built from source for now. Pre-built downloads are planned.

## Step 1: Install the build tools

**Windows**

1. Install [Rust](https://rustup.rs) by downloading and running `rustup-init.exe`.
2. When the installer offers to install the Visual Studio C++ Build Tools,
   accept. Open Beam needs a C++ compiler.
3. Install [Git for Windows](https://git-scm.com/download/win).

**macOS**

1. Open Terminal and install the Apple command line tools:

   ```bash
   xcode-select --install
   ```

2. Install Rust:

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

**Linux (Debian or Ubuntu)**

1. Install a compiler and Git:

   ```bash
   sudo apt install build-essential git
   ```

2. Install Rust:

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

Close and reopen your terminal afterwards so the `cargo` command is found.

## Step 2: Download Open Beam

```bash
git clone https://github.com/Klydo131/Open-Beam.git
```

```bash
cd Open-Beam
```

## Step 3: Build it

```bash
cargo build --release
```

The first build compiles the DuckDB engine and can take 15 minutes or more.
This happens once; later builds take seconds.

## Step 4: Check it works

```bash
cargo run --release -- --version
```

You should see `beam 0.2.0`.

Next: [Start Open Beam and see your data](02-first-dataset.md).

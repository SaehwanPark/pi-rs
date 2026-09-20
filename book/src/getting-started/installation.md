# Install rupi

This guide assumes you can open a terminal and edit a JSON file. You do not need Node.js,
Python, or a previous coding-agent harness for the normal local-model workflow.

## Option A: install from source

Install Rust 1.85 or newer from [rustup](https://rustup.rs/), then run:

```bash
git clone https://github.com/SaehwanPark/rupi.git
cd rupi
cargo install --path .
rupi --help
```

`cargo install --path .` puts the `rupi` executable in Cargo's binary directory. If your
shell cannot find it, add `~/.cargo/bin` to `PATH` (on Windows, `%USERPROFILE%\\.cargo\\bin`).

To build without installing:

```bash
cargo build --release
# target/release/rupi     (Linux/macOS)
# target\\release\\rupi.exe (Windows)
```

## Option B: download a release binary

Open the [rupi releases](https://github.com/SaehwanPark/rupi/releases) page and download
the archive matching your operating system and CPU. The v0.2.1 release includes a Windows
`x86_64-pc-windows-msvc` archive; use the source build when no matching archive is listed.

On Windows PowerShell:

```powershell
Expand-Archive .\rupi-v0.2.1-x86_64-pc-windows-msvc.zip `
  -DestinationPath $env:USERPROFILE\.cargo\bin
rupi --help
```

On Linux or macOS, unpack the archive and put `rupi` somewhere on your `PATH`.

## Optional pieces

- **llama.cpp** is needed only when you want the local Qwen example in the next guide.
- **Node.js 22.6+** is needed only for an explicitly activated Pi TypeScript extension.
- **MCP servers** are optional and are activated one at a time.

## If installation fails

- `cargo: command not found`: install Rust with rustup and restart the terminal.
- `rupi: command not found`: add Cargo's binary directory to `PATH`, or run
  `cargo run -- run ...` from the checkout.
- Windows blocks the downloaded executable: use the file's Properties dialog to unblock
  it, or build from source.

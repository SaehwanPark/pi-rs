# Install rupi

This guide assumes you can open a terminal and edit a JSON file. You do not need Node.js,
Python, or a previous coding-agent harness for the normal local-model workflow.

## Option A: use the release installer

The installers download the archive for your platform, verify its published SHA-256
checksum, and install only for your user. No administrator access or Rust toolchain is
needed.

### Linux and macOS

The POSIX installer supports Linux x86_64 and macOS Intel/Apple Silicon. It defaults to
`~/.local/bin` and does not edit shell startup files:

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/SaehwanPark/rupi/main/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
rupi --help
```

To inspect the script first, download it instead of piping it to `sh`:

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/SaehwanPark/rupi/main/install.sh \
  -o install-rupi.sh
less install-rupi.sh
sh install-rupi.sh --version v0.2.2 --install-dir "$HOME/.local/bin"
rm install-rupi.sh
```

Use `--version v0.2.2` to pin this release. `--install-dir DIRECTORY` and the equivalent
`RUPI_INSTALL_DIR` environment variable are useful for a managed or repository-local
installation. The script uses `curl` or `wget`, plus `sha256sum`, `shasum`, or `openssl`.

### Windows PowerShell

The PowerShell installer supports Windows x86_64 and defaults to
`$env:LOCALAPPDATA\rupi\bin`:

```powershell
Invoke-WebRequest https://raw.githubusercontent.com/SaehwanPark/rupi/main/install.ps1 -OutFile .\install-rupi.ps1
Get-Content .\install-rupi.ps1
.\install-rupi.ps1 -Version v0.2.2 -AddToPath
Remove-Item .\install-rupi.ps1
rupi --help
```

`-AddToPath` updates the user-level PATH; it does not require administrator access. Omit
it to choose your own PATH entry with `-InstallDir`. For a quick latest-release install,
review the script and run it with `irm https://raw.githubusercontent.com/SaehwanPark/rupi/main/install.ps1 | iex`.

The [GitHub Releases](https://github.com/SaehwanPark/rupi/releases) page contains the
archives and checksum sidecars if you prefer to download them manually. The release
workflow currently publishes:

| Platform | Target archive |
| :--- | :--- |
| Linux x86_64 | `rupi-v0.2.2-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Intel | `rupi-v0.2.2-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `rupi-v0.2.2-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `rupi-v0.2.2-x86_64-pc-windows-msvc.zip` |

## Option B: install from source

Install Rust 1.85 or newer from [rustup](https://rustup.rs/), then run:

```bash
git clone https://github.com/SaehwanPark/rupi.git
cd rupi
cargo install --path . --locked
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

## Optional pieces

- **llama.cpp** is needed only when you want the local Qwen example in the next guide.
- **Node.js 22.6+** is needed only for an explicitly activated Pi TypeScript extension.
- **MCP servers** are optional and are activated one at a time.

## If installation fails

- `curl: command not found` or `wget: command not found`: install one of those small
  download tools with your operating system package manager.
- `cargo: command not found`: install Rust with rustup and restart the terminal.
- `rupi: command not found`: open a new shell after changing `PATH`, or run
  `cargo run -- run ...` from the checkout.
- No matching prebuilt target: use the source build or download a matching archive from
  the releases page.
- Windows blocks the downloaded executable: use the file's Properties dialog to unblock
  it, or build from source.

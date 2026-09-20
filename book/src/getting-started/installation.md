# Installation

`rupi` is built in Rust using stable toolchains. Standard agent operations require no Node.js or Python runtime; those are optional integrations.

---

## Prerequisites

- **Rust 1.85+** (2024 edition):
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

---

## Building from Source

Clone the repository and compile using Cargo:

```bash
git clone https://github.com/SaehwanPark/rupi.git
cd rupi
cargo build --release
```

The optimized binary will be located at `target/release/rupi`.

To install the binary into your Cargo binary directory (`~/.cargo/bin`):

```bash
cargo install --path .
```

Verify your installation:

```bash
rupi --help
```

---

## Prebuilt Binaries (GitHub Releases)

Release artifacts are target-specific. The v0.2.0 release includes an
`x86_64-pc-windows-msvc` archive; build from source for other targets:

👉 **[Download from GitHub Releases](https://github.com/SaehwanPark/rupi/releases)**

On Windows PowerShell:

```powershell
Expand-Archive .\rupi-v0.2.0-x86_64-pc-windows-msvc.zip -DestinationPath $env:USERPROFILE\.cargo\bin
```

On Linux or macOS, use the Cargo source build above unless a matching release asset is
listed for your target.

---

## Optional Subsystems

- **Node.js (v22.6+)**: Required only if you intend to execute JavaScript/TypeScript Pi extensions via the optional extension host. The host is not started for ordinary sessions.
- **MCP Servers**: Any standard Model Context Protocol server communicating over stdio.

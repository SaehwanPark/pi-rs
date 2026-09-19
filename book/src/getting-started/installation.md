# Installation

`pi-rs` is built in Rust using stable toolchains. Standard agent operations require no Node.js or Python runtime; those are optional integrations.

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
git clone https://github.com/SaehwanPark/pi-rs.git
cd pi-rs
cargo build --release
```

The optimized binary will be located at `target/release/pi-rs`.

To install the binary into your Cargo binary directory (`~/.cargo/bin`):

```bash
cargo install --path .
```

Verify your installation:

```bash
pi-rs --help
```

---

## Prebuilt Binaries (GitHub Releases)

Release artifacts are published on GitHub when available. Check the release notes for the supported target, or build from source for another platform:

👉 **[Download from GitHub Releases](https://github.com/SaehwanPark/pi-rs/releases)**

After downloading:

```bash
tar -xzf pi-rs-v0.2.0-x86_64-unknown-linux-gnu.tar.gz
chmod +x pi-rs
sudo mv pi-rs /usr/local/bin/
```

---

## Optional Subsystems

- **Node.js (v22.6+)**: Required only if you intend to execute JavaScript/TypeScript Pi extensions via the optional extension host. The host is not started for ordinary sessions.
- **MCP Servers**: Any standard Model Context Protocol server communicating over stdio.

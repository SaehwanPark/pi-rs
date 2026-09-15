# Installation

`pi-rs` is built in Rust using stable toolchains. It has zero required external runtime dependencies for core agent operations (no Node.js or Python required for standard tasks).

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

Precompiled binaries for Linux (x86_64, aarch64) and macOS (Apple Silicon, Intel) are published with every tagged release on GitHub:

👉 **[Download from GitHub Releases](https://github.com/SaehwanPark/pi-rs/releases)**

After downloading:

```bash
tar -xzf pi-rs-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
chmod +x pi-rs
sudo mv pi-rs /usr/local/bin/
```

---

## Optional Subsystems

- **Node.js (v20+)**: Required only if you intend to execute complex JavaScript/TypeScript Pi extensions via the optional extension host.
- **MCP Servers**: Any standard Model Context Protocol server communicating over stdio.

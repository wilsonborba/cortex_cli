# Cortex CLI

Cortex in the terminal for Linux and macOS. This first build provides help and
version commands. Chat, sign-in and provider support follow in separate changes.

## Build and install

Install Rust 1.85 or newer with Cargo, rustfmt and Clippy. From this repository:

```sh
cargo build --locked --release
cargo install --locked --path .
cortex --help
cortex --version
```

Cargo installs `cortex` into its bin directory. Add that directory to your `PATH`.
No account or provider credential is needed for these commands.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
```

Run the same commands on the owner Mac with its native Rust toolchain. Apple
silicon uses `aarch64-apple-darwin`; Intel Macs use `x86_64-apple-darwin`.
CI checks Linux and both macOS architectures. CI does not publish releases.

## Structure

`src/core` holds settings and diagnostics. `src/dal` holds local and remote data
adapters. `src/domain` holds business models and services. `src/presentation/cli`
separates commands, handlers, formatters and machine outputs. `src/app.rs` composes
the application, and `src/main.rs` starts it.

The CLI writes ordinary terminal output. It does not enter an alternate screen.

# Cortex CLI

Cortex in the terminal for Linux and macOS. Sign in through the normal Asodya
browser flow. Chat and external provider support follow in separate changes.

## Build and install

Install Rust 1.91.1 or newer with Cargo, rustfmt and Clippy. From this repository:

```sh
cargo build --locked --release
cargo install --locked --path .
cortex --help
cortex --version
```

Cargo installs `cortex` into its bin directory. Add that directory to your `PATH`.
No account or provider credential is needed for these commands.

Linux builds also need the D-Bus development package and `pkg-config`. Debian and
Ubuntu provide these as `libdbus-1-dev` and `pkg-config`. macOS uses the OS
Keychain. The HTTP and app-context adapters use Rust cryptography.

## Sign in

```sh
cortex login
cortex logout
```

Open the printed link. Check that the browser shows the same four-character code.
Sign in, return to the terminal and press Enter. Each Enter sends one Finish
request. The CLI does not poll. If login is not finished, complete it in the
browser and press Enter again. Type `cancel` or send end-of-input to cancel the
pending login through the gateway.

The success message shows the approximate login location when available. A country
code is used when no localized country name is available. Unknown location shows
`Logged in`.

The OS keyring stores only this device session. Linux needs an unlocked Secret
Service keyring; macOS uses Keychain. The CLI never saves credentials in a plain
file. If secure storage is unavailable, the session stays in process memory and
the next process must sign in again. `logout` revokes only the current session.
If the gateway does not confirm logout, credentials remain for an explicit retry.

Asodya sign-in does not grant access to external provider subscriptions. There is
no Asodya API-key login option. Native login must be enabled in the deployed
gateway and auth application. Local contract tests do not verify that deployment.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
```

Run the same commands on macOS with its native Rust toolchain. Apple
silicon uses `aarch64-apple-darwin`; Intel Macs use `x86_64-apple-darwin`.
On macOS with Rust 1.91.1 installed:

```sh
rustup run 1.91.1 cargo fmt --all -- --check
rustup run 1.91.1 cargo clippy --locked --all-targets -- -D warnings
rustup run 1.91.1 cargo test --locked --all-targets
rustup run 1.91.1 cargo build --locked --release
./target/release/cortex --help
./target/release/cortex --version
```

Use a native Apple silicon or Intel Mac for its architecture. A Linux build does
not verify macOS linking or runtime behavior.

## Structure

`src/core` holds settings and diagnostics. `src/dal` holds local and remote data
adapters. `src/domain` holds business models and services. `src/presentation/cli`
separates commands, handlers, formatters and machine outputs. `src/app.rs` composes
the application, and `src/main.rs` starts it.

The CLI writes ordinary terminal output. It does not enter an alternate screen.

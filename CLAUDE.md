# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
# Build the library
cargo build

# Build with release optimizations
cargo build --release

# Run tests (if any are added)
cargo test

# Run a specific test
cargo test <test_name>

# Check that code compiles without producing binaries
cargo check
```
## Clippy
After each code modification, you must run
```bash
cargo clippy
```


## Architecture

This is a single-file Rust library (`src/lib.rs`) providing an asynchronous TCP client built on the **actor pattern** via `async-channel`.

### Actor-based write path

On connection, two tokio tasks are spawned:

1. **Reader task** — executes the user-provided closure `input`, passing it the read half of the split TCP stream. The closure's return value (`Ok(true)` / `Ok(false)`) controls whether to disconnect after the closure completes.
2. **Writer task** — consumes `State` messages from a bounded `async_channel` (capacity 4096) and performs the corresponding write operations on the write half of the stream.

The `State` enum encodes four write operations:
- `Disconnect` — shuts down the writer and exits the writer task
- `Send(Vec<u8>)` — writes data without flushing
- `SendFlush(Vec<u8>)` — writes data then flushes
- `Flush` — flushes without writing

### Generic stream type

`TcpClient<T>` is generic over `T: AsyncRead + AsyncWrite + Send + Sync + 'static`. The concrete `TcpClient<TcpStream>` provides a simple `connect()` method. The generic `connect_stream_type()` allows wrapping the raw `TcpStream` (e.g., TLS upgrade) before the actor loop starts — the caller provides a `stream_init` closure that transforms `TcpStream → T`.

### Thread safety

`TcpClient` is always used inside `Arc<TcpClient<T>>`. The `connect`/`connect_stream_type` methods return `Result<Arc<TcpClient<T>>>`. All public methods (`send`, `send_all`, `flush`, `disconnect`) take `&self` (not `&mut self`) and route work through the channel sender, making them safe to call from multiple tasks.

An `AtomicBool` (`disconnect`) guards against repeated disconnect attempts.

## Dependencies

- **tokio** (`rt`, `net`, `io-util` features) — async runtime and TCP I/O
- **async-channel** — bounded MPSC channel for the actor queue
- **anyhow** — error handling
- **log** — logging (consumers choose the logger backend, e.g., `env_logger`)

## Code conventions

- The entire library is a single file; there is no `main.rs` or binary target.
- Edition 2018 Rust.
- Heavy use of `#[inline]` on public async methods.
- Generic parameters use single-letter names with where-clause bounds.

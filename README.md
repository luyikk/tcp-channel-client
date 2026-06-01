# tcp-channel-client

[![Crates.io](https://img.shields.io/crates/v/tcp-channel-client)](https://crates.io/crates/tcp-channel-client)
[![Documentation](https://docs.rs/tcp-channel-client/badge.svg)](https://docs.rs/tcp-channel-client)
[![License](https://img.shields.io/crates/l/tcp-channel-client)](LICENSE)

An asynchronous TCP client built on the **actor pattern** using `async-channel` and `tokio`.

On connection, the library spawns two tokio tasks: a reader task that runs user-provided logic, and a writer task that consumes write commands from a bounded channel — decoupling reads from writes without locks.

## Installation

Add to `Cargo.toml`:

```toml
[dependencies]
tcp-channel-client = "0.2"
```

## Usage

```rust
use tcp_channel_client::TcpClient;
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Connect to a TCP server
    let client = TcpClient::connect(
        "127.0.0.1:5555",
        |_, client, mut reader| async move {
            let mut buf = vec![0u8; 1024];
            loop {
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                // Echo received data back
                client.send(buf[..n].to_vec()).await?;
            }
            Ok(true) // disconnect after loop ends
        },
        (), // user token passed to the closure
    )
    .await?;

    // Send data from anywhere that holds an Arc clone
    client.send(b"hello".to_vec()).await?;

    // Graceful disconnect
    client.disconnect().await?;

    Ok(())
}
```

## API

### `TcpClient::connect(addr, input, token)`

Connect to `addr` using a plain `TcpStream`. Returns `Arc<TcpClient<TcpStream>>`.

### `TcpClient::connect_stream_type(addr, stream_init, input, token)`

Connect and transform the raw `TcpStream` before entering the actor loop (e.g., TLS upgrade via `tokio-native-tls`). Returns `Arc<TcpClient<T>>`.

### Methods on `TcpClient<T>`

| Method | Description |
|---|---|
| `send(buf)` | Enqueue a write without flushing |
| `send_all(buf)` | Enqueue a write followed by a flush |
| `flush()` | Enqueue a flush |
| `disconnect()` | Gracefully shut down the writer task and close the connection |

All methods take `&self` and are safe to call concurrently from multiple tasks.

### The `input` closure

```rust
FnOnce(A, Arc<TcpClient<T>>, ReadHalf<T>) -> Future<Output = Result<bool>>
```

- `A` — the user-provided token
- `Arc<TcpClient<T>>` — a clone of the client handle, usable for sending from within the reader
- `ReadHalf<T>` — the read half of the split stream

Return `Ok(true)` to trigger disconnect after the closure completes, or `Ok(false)` to leave the connection open.

## How It Works

```
                  ┌─────────────────┐
 user code ──────►│  async_channel  │──────► writer task ──► write half
   ▲              │   (bounded 4096) │                         │
   │              └─────────────────┘                         │
   │                                                         │
   └──────────── reader task ◄── read half ◄──────────────────┘
```

Write commands (`Send`, `SendFlush`, `Flush`, `Disconnect`) are sent through the channel and processed sequentially by the writer task. The reader task runs the user-supplied closure independently, reading from the stream.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
use tcp_channel_client::TcpClient;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// 启动一个本地 TCP echo 服务器，返回其监听地址。
/// 服务器对每个连接读取数据并原样写回。
async fn spawn_echo_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut reader, mut writer) = stream.split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });

    addr
}

/// 发送一条消息到 echo 服务器，验证 reader 闭包能收到回显。
#[tokio::test]
async fn echo_send_and_receive() {
    let addr = spawn_echo_server().await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);

    let client = TcpClient::connect(
        addr,
        move |_, _client, mut reader| async move {
            let mut buf = [0u8; 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = tx.send(buf[..n].to_vec()).await;
                    }
                    Err(_) => break,
                }
            }
            Ok(false)
        },
        (),
    )
    .await
    .unwrap();

    // 发送数据，等待 echo 返回
    client.send_all(b"hello echo".to_vec()).await.unwrap();
    let echoed = rx.recv().await.unwrap();
    assert_eq!(echoed, b"hello echo");

    client.disconnect().await.unwrap();
}

/// 发送空数据，验证 reader 不会收到任何内容（0 字节写入不产生 TCP 报文）。
#[tokio::test]
async fn send_all_empty_buffer() {
    let addr = spawn_echo_server().await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<usize>(1);

    let client = TcpClient::connect(
        addr,
        move |_, _client, mut reader| async move {
            let mut buf = [0u8; 1024];
            // 等待服务端或客户端关闭，不期望收到空数据
            let n = reader.read(&mut buf).await.unwrap_or(0);
            let _ = tx.send(n).await;
            Ok(false)
        },
        (),
    )
    .await
    .unwrap();

    // 发送空 buffer — 不应该在 wire 上产生数据
    client.send_all(vec![]).await.unwrap();

    // 发送实际数据以触发 reader 中的 read
    client.send_all(b"ping".to_vec()).await.unwrap();

    // reader 只应该收到 "ping" 的 echo，不会包括空 buffer 的内容
    let n = rx.recv().await.unwrap();
    assert_eq!(n, 4); // "ping" = 4 bytes，不包括空 buffer

    client.disconnect().await.unwrap();
}

/// 验证 disconnect 后 write 操作返回错误。
#[tokio::test]
async fn write_after_disconnect_fails() {
    let addr = spawn_echo_server().await;

    let client = TcpClient::connect(
        addr,
        |_, _client, mut reader| async move {
            let mut buf = [0u8; 64];
            let _ = reader.read(&mut buf).await;
            Ok(false)
        },
        (),
    )
    .await
    .unwrap();

    client.disconnect().await.unwrap();

    // 断开后 try_send 应失败
    assert!(client.try_send(b"after disconnect".to_vec()).is_err());
}

/// 验证 reader 闭包返回 Err 后连接被断开。
#[tokio::test]
async fn reader_error_triggers_disconnect() {
    let addr = spawn_echo_server().await;

    let client = TcpClient::connect(
        addr,
        |_, _client, mut reader| async move {
            // 先发送一条消息表示 reader 已启动
            let _ = reader.read(&mut [0u8; 1]).await;
            // 返回 Err 触发断开
            Err(anyhow::anyhow!("simulated reader error"))
        },
        (),
    )
    .await
    .unwrap();

    // 触发 reader 中的 read（发送任意数据让 echo 回显触发 reader.read）
    client.send_all(b"x".to_vec()).await.unwrap();

    // 等待 reader 处理错误并触发 disconnect
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 此时 disconnect 应该已经被触发
    assert!(client.try_send(b"post-error".to_vec()).is_err());
}

/// 验证多客户端共享一个 echo 服务器的并发场景。
#[tokio::test]
async fn multi_client_concurrent_send() {
    let addr = spawn_echo_server().await;

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let addr = addr;
            tokio::spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
                let client = TcpClient::connect(
                    addr,
                    move |_, _client, mut reader| async move {
                        let mut buf = [0u8; 256];
                        loop {
                            match reader.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    let _ = tx.send(buf[..n].to_vec()).await;
                                }
                                Err(_) => break,
                            }
                        }
                        Ok(false)
                    },
                    (),
                )
                .await
                .unwrap();

                let msg = format!("client-{}", i);
                client.send_all(msg.clone().into_bytes()).await.unwrap();
                let echoed = rx.recv().await.unwrap();
                client.disconnect().await.unwrap();
                (msg.into_bytes(), echoed)
            })
        })
        .collect();

    for h in handles {
        let (sent, received) = h.await.unwrap();
        assert_eq!(sent, received);
    }
}

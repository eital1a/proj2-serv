// proj2-serv/src/main.rs
// Tokio-based high-throughput TCP + UDP server.
// Listens: TCP 0.0.0.0:8080, UDP 0.0.0.0:7070
// Uses large I/O buffers, burst sends, multiple ACKs and a probe to make UDP uploads reliable.

use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::time::{Duration, Instant};
use std::io::ErrorKind;
use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};
use std::sync::Arc;
use socket2::{Socket, Domain, Type, Protocol};
use std::collections::HashMap;
use tokio::sync::Mutex;
use tokio::task;
use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create and tune the UDP socket via socket2, then convert to Tokio UdpSocket.
    let udp_sock = {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("creating socket2 UDP socket")?;
        // Increase buffers (example: 8 MiB)
        let buf = 8 * 1024 * 1024;
        let _ = s.set_recv_buffer_size(buf);
        let _ = s.set_send_buffer_size(buf);
        s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 7070)).into())
            .context("binding UDP socket")?;
        let std_udp: std::net::UdpSocket = s.into();
        std_udp.set_nonblocking(true).context("set_nonblocking UDP")?;
        UdpSocket::from_std(std_udp).context("convert to tokio UdpSocket")?
    };
    let udp_socket = Arc::new(udp_sock);
    println!("UDP server listening on 0.0.0.0:7070");

    // Create and tune TCP listener via socket2
    let tcp_listener = {
        let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
            .context("creating socket2 TCP socket")?;
        let buf = 4 * 1024 * 1024;
        let _ = s.set_recv_buffer_size(buf);
        let _ = s.set_send_buffer_size(buf);
        let _ = s.set_reuse_address(true);
        s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8080)).into())
            .context("binding TCP listener")?;
        s.listen(1024).context("listen on TCP socket")?;
        let std_listener: std::net::TcpListener = s.into();
        std_listener.set_nonblocking(true).context("set_nonblocking TCP listener")?;
        TcpListener::from_std(std_listener).context("convert to tokio TcpListener")?
    };
    println!("TCP server listening on 0.0.0.0:8080");

    // Run TCP and UDP loops concurrently
    let udp_task = run_udp_server(udp_socket.clone());
    let tcp_task = run_tcp_server(tcp_listener);
    tokio::try_join!(udp_task, tcp_task)?;
    Ok(())
}

async fn run_tcp_server(listener: TcpListener) -> anyhow::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                println!("New TCP connection from {}", addr);
                tokio::spawn(async move {
                    if let Err(e) = handle_tcp_client(stream, addr).await {
                        eprintln!("TCP client {} error: {:?}", addr, e);
                    }
                });
            }
            Err(e) => {
                eprintln!("TCP accept error: {:?}", e);
                // small sleep to avoid busy loop on persistent accept errors
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

async fn handle_tcp_client(mut stream: TcpStream, peer: SocketAddr) -> anyhow::Result<()> {
    let _ = stream.set_nodelay(true);
    const BUF_SIZE: usize = 64 * 1024;
    let mut read_buf = vec![0u8; BUF_SIZE];
    loop {
        let n = match stream.read(&mut read_buf).await {
            Ok(0) => {
                println!("TCP client {} disconnected", peer);
                return Ok(());
            }
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                println!("TCP client {} reset connection", peer);
                return Ok(());
            }
            Err(e) => {
                eprintln!("TCP read error from {}: {:?}", peer, e);
                return Err(e.into());
            }
        };
        let command = String::from_utf8_lossy(&read_buf[..n]).trim().to_string();
        println!("TCP server received from {}: {}", peer, command);

        if command.starts_with("START_DOWNLOAD") {
            let payload = vec![0u8; BUF_SIZE];
            let start = Instant::now();
            let mut sent_bytes: usize = 0usize;
            while start.elapsed() < Duration::from_secs(5) {
                if let Err(e) = stream.write_all(&payload).await {
                    if e.kind() == ErrorKind::BrokenPipe || e.kind() == ErrorKind::ConnectionReset {
                        println!("Client {} closed connection during download", peer);
                        break;
                    } else {
                        eprintln!("TCP write error to {}: {:?}", peer, e);
                        break;
                    }
                }
                sent_bytes += payload.len();
            }
            println!("TCP server finished sending download to {} (~{} bytes)", peer, sent_bytes);
        } else if command.starts_with("START_UPLOAD") {
            let start = Instant::now();
            let mut total_rx: usize = 0usize;
            while start.elapsed() < Duration::from_secs(5) {
                match stream.read(&mut read_buf).await {
                    Ok(0) => break,
                    Ok(m) => total_rx += m,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        tokio::task::yield_now().await;
                    }
                    Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                        println!("Client reset connection during upload: {}", peer);
                        break;
                    }
                    Err(e) => {
                        eprintln!("TCP read error during upload from {}: {:?}", peer, e);
                        break;
                    }
                }
            }
            println!("TCP server received {} bytes during upload from {}", total_rx, peer);
        } else {
            println!("TCP server: unknown command from {}: {:?}", peer, command);
        }
    }
}

async fn run_udp_server(udp_socket: Arc<UdpSocket>) -> anyhow::Result<()> {
    const PAYLOAD_SIZE: usize = 1400; // MTU-friendly
    let send_payload = vec![0u8; PAYLOAD_SIZE];
    let mut recv_buf = vec![0u8; 64 * 1024];

    // Active uploads: client -> (deadline, total_bytes)
    let active_uploads: Arc<Mutex<HashMap<std::net::SocketAddr, (Instant, usize)>>> =
        Arc::new(Mutex::new(HashMap::new()));

    loop {
        match udp_socket.recv_from(&mut recv_buf).await {
            Ok((len, addr)) => {
                let msg = String::from_utf8_lossy(&recv_buf[..len]).trim().to_string();
                println!("UDP server received from {}: {}", addr, msg);

                // Replace existing START_DOWNLOAD handling with this block
                if msg.starts_with("START_DOWNLOAD") {
                    // Immediately ACK so client knows we saw the request
                    // (send a few ACKs to be robust)
                    const ACKS: usize = 3;
                    const ACK_INTERVAL_MS: u64 = 10;
                    for _ in 0..ACKS {
                        if let Err(e) = udp_socket.send_to(b"ACK_DOWNLOAD", &addr).await {
                            eprintln!("UDP send ACK_DOWNLOAD failed to {}: {:?}", addr, e);
                        }
                        tokio::time::sleep(Duration::from_millis(ACK_INTERVAL_MS)).await;
                    }

                    // Spawn an async task that sends bursts using the shared udp_socket.
                    // This avoids creating per-client blocking sockets and keeps the runtime efficient.
                    let sock = udp_socket.clone();
                    let dest = addr;
                    let payload = send_payload.clone(); // 1400 bytes
                    tokio::spawn(async move {
                        const BURST: usize = 16; // tune 4..32
                        const BACKOFF_US: u64 = 20; // microsecond backoff on WouldBlock
                        let start = Instant::now();
                        let mut sent_bytes: usize = 0usize;

                        while start.elapsed() < Duration::from_secs(5) {
                            // send a burst of datagrams
                            let mut any_sent = false;
                            for _ in 0..BURST {
                                match sock.send_to(&payload, &dest).await {
                                    Ok(n) => {
                                        sent_bytes += n;
                                        any_sent = true;
                                    }
                                    Err(e) => {
                                        // backpressure: wait a tiny bit and break the burst
                                        if e.kind() == std::io::ErrorKind::WouldBlock {
                                            tokio::time::sleep(Duration::from_micros(BACKOFF_US)).await;
                                            break;
                                        } else {
                                            eprintln!("UDP send_to error to {}: {:?}", dest, e);
                                            tokio::time::sleep(Duration::from_micros(BACKOFF_US)).await;
                                            break;
                                        }
                                    }
                                }
                            }

                            // Minimal yield: only yield if we actually sent something.
                            // This keeps the task responsive without throttling throughput.
                            if any_sent {
                                tokio::task::yield_now().await;
                            } else {
                                tokio::time::sleep(Duration::from_micros(BACKOFF_US)).await;
                            }
                        }

                        println!("UDP server finished sending download to {} (~{} bytes)", dest, sent_bytes);
                    });
                    continue;
                }
                else if msg.starts_with("START_UPLOAD") {
                    // register an upload window for this addr and ACK (insert first)
                    let deadline = Instant::now() + Duration::from_secs(5);
                    {
                        let mut map = active_uploads.lock().await;
                        map.insert(addr, (deadline, 0));
                    }

                    // Send multiple ACKs and a tiny probe to prime NATs/middleboxes
                    const ACKS: usize = 3;
                    const ACK_INTERVAL_MS: u64 = 20;
                    for _ in 0..ACKS {
                        if let Err(e) = udp_socket.send_to(b"ACK_UPLOAD", &addr).await {
                            eprintln!("UDP send ACK failed to {}: {:?}", addr, e);
                        }
                        tokio::time::sleep(Duration::from_millis(ACK_INTERVAL_MS)).await;
                    }
                    // tiny probe to help NAT learn mapping
                    if let Err(e) = udp_socket.send_to(b"P", &addr).await {
                        eprintln!("UDP send probe failed to {}: {:?}", addr, e);
                    } else {
                        println!("UDP server registered upload window for {} until {:?}", addr, deadline);
                    }
                } else {
                    // Non-control datagram: count toward active upload if present
                    let now = Instant::now();
                    let mut map = active_uploads.lock().await;
                    if let Some((deadline, total)) = map.get_mut(&addr) {
                        if now <= *deadline {
                            *total += len;
                        } else {
                            // expired: report and remove
                            println!("UDP server received {} bytes during upload from {} (final)", *total, addr);
                            map.remove(&addr);
                        }
                    } else {
                        // Unexpected payload; ignore or log for debug
                        println!("UDP payload from {}: {} bytes (no active window)", addr, len);
                    }

                    // Sweep expired entries and report
                    let now = Instant::now();
                    let expired: Vec<std::net::SocketAddr> = map
                        .iter()
                        .filter_map(|(client, (deadline, _))| if now > *deadline { Some(*client) } else { None })
                        .collect();
                    for client in expired {
                        if let Some((_, total)) = map.remove(&client) {
                            println!("UDP server received {} bytes during upload from {}", total, client);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("UDP recv_from error: {:?}", e);
                // small sleep to avoid busy-looping on persistent errors
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

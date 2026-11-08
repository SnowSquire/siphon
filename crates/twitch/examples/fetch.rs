//! Throwaway smoke test for the compio runtime, the completion IO path and (later)
//! the custom HTTP client. Not part of the shipped binary.
//!
//! `cargo run -p twitch-hls-client2 --example fetch -- http://127.0.0.2:8000/index.m3u8`

use std::io::Write;

use compio::io::AsyncRead;
use compio::io::AsyncWriteExt;
use compio::net::TcpStream;

#[compio::main]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.2:8000/index.m3u8".to_owned());

    let Some(rest) = url.strip_prefix("http://") else {
        eprintln!("only plain http is wired up in this smoke test: {url}");
        std::process::exit(1);
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().expect("port")),
        None => (authority, 80),
    };

    let mut stream = TcpStream::connect((host, port)).await.expect("connect");
    stream.set_nodelay(true).expect("nodelay");
    println!(
        "connected {} -> {:?}",
        authority,
        stream.peer_addr().expect("peer addr")
    );

    let request = format!(
        "GET /{path} HTTP/1.1\r\nHost: {authority}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    let result = stream.write_all(request).await;
    result.0.expect("send request");

    let mut received = 0usize;
    let mut buffer = Vec::with_capacity(64 * 1024);
    loop {
        let result = stream.read(buffer).await;
        let read = result.0.expect("read");
        buffer = result.1;
        if read == 0 {
            break;
        }
        received += read;
        std::io::stdout()
            .write_all(&buffer[..read])
            .expect("stdout");
    }

    eprintln!("\n--- {received} bytes");
}

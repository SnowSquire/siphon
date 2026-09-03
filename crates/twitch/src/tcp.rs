use bytes::Buf;

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::thread::JoinHandle;
use std::time::Instant;

use std::thread;

use crate::scheduler::{Segment, SegmentMessage};

use crate::constants::DEFAULT_PORT;

pub fn spawn_tcp_listener(
    add_readers: kanal::Sender<kanal::Sender<SegmentMessage>>,
    start: Instant,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("tcp listener"))
        .spawn(move || {
            let mut client_threads = Vec::new();
            const ADDRS: [std::net::SocketAddr; 4] = [
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), DEFAULT_PORT),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            ];
            let listener = TcpListener::bind(&ADDRS[..]).unwrap();

            println!("{}", listener.local_addr().unwrap().to_string());

            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let start = start.clone();
                        let (reader_rx, handle) = spawn_tcp_client_thread(stream, start);
                        _ = add_readers.send(reader_rx);
                        client_threads.push(handle);
                    }
                    Err(e) => eprintln!("Connection failed: {}", e),
                }
            }
        })
        .expect("failed to spawn listener thread")
}

fn spawn_tcp_client_thread(
    stream: std::net::TcpStream,
    start: Instant,
) -> (kanal::Sender<SegmentMessage>, JoinHandle<()>) {
    //TODO: probably make this bounded and drop slow connections
    let (reader_rx, reader_tx): (
        kanal::Sender<SegmentMessage>,
        kanal::Receiver<SegmentMessage>,
    ) = kanal::unbounded();
    let handle = thread::Builder::new()
        .name(format!(
            "tcp sender {0}",
            stream.peer_addr().unwrap().to_string()
        ))
        .spawn(move || {
            let mut stream = stream;
            stream
                .set_nodelay(true)
                .expect("Failed to set TCP no delay");

            let mut last_sequence_num = 0;
            for msg in reader_tx {
                match msg {
                    SegmentMessage::Init { mut reader } => {
                        while reader.wait_for_data() {
                            let buf = reader.chunk();

                            let written = match stream.write(buf) {
                                Ok(w) => w,
                                Err(e) => {
                                    eprintln!("Write error: {e}");
                                    return;
                                }
                            };

                            if written == 0 {
                                break;
                            }

                            reader.advance(written);
                        }
                    }
                    SegmentMessage::Segment(Segment {
                        mut reader,
                        sequence_num,
                        ..
                    }) => {
                        if last_sequence_num >= sequence_num {
                            continue;
                        }
                        last_sequence_num = sequence_num;
                        loop {
                            let buf = reader.chunk();

                            let written = match stream.write_all(buf) {
                                Ok(w) => w,
                                Err(e) => {
                                    eprintln!("Write error: {e}");
                                    return;
                                }
                            };

                            reader.advance(buf.len());
                            if !reader.wait_for_data() {
                                break;
                            }
                        }
                    }
                    SegmentMessage::Prefetch {
                        expected_sequence,
                        mut reader,
                    } => {
                        if last_sequence_num >= expected_sequence {
                            continue;
                        }
                        last_sequence_num = expected_sequence;
                        loop {
                            let buf = reader.chunk();

                            let written = match stream.write_all(buf) {
                                Ok(w) => w,
                                Err(e) => {
                                    eprintln!("Write error: {e}");
                                    return;
                                }
                            };

                            reader.advance(buf.len());
                            if !reader.wait_for_data() {
                                break;
                            }
                        }
                    }
                    SegmentMessage::Discontinuity() => last_sequence_num = 0,
                }
            }
        })
        .expect("failed to spawn tcp client thread");
    // if this channel closes should lowkey just restart the whole process
    (reader_rx, handle)
}

use std::io::Write;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::thread::{self};

use crate::constants::DEFAULT_PORT;
use crate::scheduler::Segment;
use crate::scheduler::SegmentMessage;

pub fn spawn_tcp_listener(
    add_readers: kanal::Sender<kanal::Sender<SegmentMessage>>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("tcp listener"))
        .spawn(move || {
            let mut client_threads = Vec::new();
            const ADDRS: [SocketAddr; 4] = [
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), DEFAULT_PORT),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            ];
            let listener = TcpListener::bind(&ADDRS[..]).unwrap();

            println!("{}", listener.local_addr().unwrap());

            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let (reader_rx, handle) = spawn_tcp_client_thread(stream);
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
) -> (kanal::Sender<SegmentMessage>, JoinHandle<()>) {
    //TODO: probably make this bounded and drop slow connections
    let (reader_rx, reader_tx): (
        kanal::Sender<SegmentMessage>,
        kanal::Receiver<SegmentMessage>,
    ) = kanal::unbounded();
    let handle = thread::Builder::new()
        .name(format!("tcp sender {0}", stream.peer_addr().unwrap()))
        .spawn(move || {
            let mut stream = stream;
            stream
                .set_nodelay(true)
                .expect("Failed to set TCP no delay");

            let mut last_sequence_num = 0;
            for msg in reader_tx {
                match msg {
                    SegmentMessage::Init { mut reader } => {
                        while let Some(buf) = reader.chunk() {
                            if stream.write_all(buf).is_err() {
                                continue;
                            };
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
                        while let Some(buf) = reader.chunk() {
                            if stream.write_all(buf).is_err() {
                                continue;
                            };
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
                        while let Some(buf) = reader.chunk() {
                            if stream.write_all(buf).is_err() {
                                continue;
                            };
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

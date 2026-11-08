use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::rc::Rc;

use compio::buf::IoBuf;
use compio::io::AsyncWriteExt;
use compio::net::TcpListener;
use compio::net::TcpStream;
use kanal::AsyncReceiver;
use tracing::error;

use crate::constants::DEFAULT_PORT;
use crate::constants::MAX_CONSUMER_LAG;
use crate::scheduler::SegmentMessage;
use crate::spool::ConsumerWake;
use crate::spool::Reader;
use crate::spool::WakeSet;

pub fn spawn_tcp_listener(
    wakes: Rc<WakeSet>,
    add_readers: kanal::Sender<kanal::Sender<SegmentMessage>>,
) {
    compio::runtime::spawn(async move {
        const ADDRS: [SocketAddr; 4] = [
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), DEFAULT_PORT),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        ];

        // compio's `bind` takes the first address that works, so each loopback
        // family gets its own listener (and its own accept loop).
        for addr in ADDRS {
            let Ok(listener) = TcpListener::bind(addr).await else {
                continue;
            };
            println!("{}", listener.local_addr().expect("local addr"));
            compio::runtime::spawn(accept_loop(
                listener,
                Rc::clone(&wakes),
                add_readers.clone(),
            ))
            .detach();
        }
    })
    .detach();
}

async fn accept_loop(
    listener: TcpListener,
    wakes: Rc<WakeSet>,
    add_readers: kanal::Sender<kanal::Sender<SegmentMessage>>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                // TODO(rtsp): slow or distant consumers are dropped on purpose.
                // Late/rate-limited viewers are meant to be served by the RTSP/RTP
                // output from disk or the DVR playlist, not by letting the live
                // fan-out buffer indefinitely. See notes/es-rtp-rtsp.md.
                // Bounded: the scheduler drops a client whose queue fills up,
                // which is the lagging-consumer policy.
                let (tx, rx) = kanal::bounded(MAX_CONSUMER_LAG);
                if add_readers.send(tx).is_err() {
                    return;
                }
                let wake = ConsumerWake::new();
                wakes.register(&wake);
                compio::runtime::spawn(client(stream, rx.to_async(), peer, wake)).detach();
            }
            Err(error) => error!(%error, "tcp accept failed"),
        }
    }
}

async fn client(
    mut stream: TcpStream,
    rx: AsyncReceiver<SegmentMessage>,
    peer: SocketAddr,
    wake: Rc<ConsumerWake>,
) {
    if let Err(error) = stream.set_nodelay(true) {
        error!(%error, %peer, "failed to set TCP no delay");
        return;
    }

    let mut last_sequence = 0;
    while let Ok(message) = rx.recv().await {
        let written = match message {
            SegmentMessage::Init { mut reader } => {
                write_reader(&mut stream, &mut reader, &wake).await
            }
            SegmentMessage::Segment(segment) => {
                if last_sequence >= segment.sequence_num {
                    continue;
                }
                last_sequence = segment.sequence_num;
                let mut reader = segment.reader;
                write_reader(&mut stream, &mut reader, &wake).await
            }
            SegmentMessage::Prefetch {
                expected_sequence,
                mut reader,
            } => {
                if last_sequence >= expected_sequence {
                    continue;
                }
                last_sequence = expected_sequence;
                write_reader(&mut stream, &mut reader, &wake).await
            }
            SegmentMessage::Discontinuity() => {
                last_sequence = 0;
                Ok(())
            }
        };

        if written.is_err() {
            return;
        }
    }
}

async fn write_reader(
    stream: &mut TcpStream,
    reader: &mut Reader,
    wake: &ConsumerWake,
) -> Result<(), ()> {
    let started = std::time::Instant::now();
    let mut views = 0usize;
    let mut bytes = 0usize;
    let mut first_ms = 0u128;
    let mut last_ms = 0u128;

    while let Some(view) = reader.next_view(wake).await {
        views += 1;
        if views == 1 {
            first_ms = started.elapsed().as_millis();
        }
        bytes += view.as_init().len();
        last_ms = started.elapsed().as_millis();
        if stream.write_all(view).await.0.is_err() {
            return Err(());
        }
    }

    tracing::debug!(views, bytes, first_ms, last_ms, "consumer segment written");
    Ok(())
}

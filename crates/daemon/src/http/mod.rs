//! Minimal HTTP/1.1 client on compio completion IO.
//!
//! Deliberately narrow: the only requests this program makes are `GET`s for
//! manifests and segments, a `POST` for the Twitch GQL token query, and a `GET`
//! for the OAuth validation endpoint. Everything else (cookies, proxies, h2,
//! content encodings) is out of scope.
//!
//! Twitch's segment CDN is HTTP/1.1 only and mostly `Transfer-Encoding: chunked`
//! with chunk sizes from 6 B to 256 KiB (see `notes/hls-transport-probe.md`), so
//! the body decoder is chunked-first, never assumes a minimum read, and reads
//! payload straight into the caller's destination.

mod body;
mod error;
mod pool;
mod tls;
mod transport;
pub mod ws;

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;
use http::Method;
pub use http::StatusCode;
use http::Uri;
use http::header::CONNECTION;
use http::header::CONTENT_LENGTH;
use http::header::LOCATION;
use http::header::TRANSFER_ENCODING;
use rustls::ClientConfig;
use rustls::RootCertStore;
use tracing::debug;

use crate::constants::MAX_HEAD_BYTES as MAX_HEAD;
use crate::constants::MAX_HEADERS;
use crate::constants::MAX_REDIRECTS;
pub use crate::http::body::Body;
use crate::http::body::Lease;
pub use crate::http::body::Mode;
pub use crate::http::error::Error;
use crate::http::pool::Pool;
use crate::http::pool::PoolKey;
use crate::http::transport::Conn;

pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Error> {
    use futures_util::future::Either;
    use futures_util::future::select;

    let future = Box::pin(future);
    let sleep = Box::pin(compio::time::sleep(duration));
    match select(future, sleep).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(_) => Err(Error::Timeout("io")),
    }
}

/// Pooled HTTP/1.1 client. One per stream; cheap to clone by reference.
pub struct Client {
    pool: Rc<RefCell<Pool>>,
    tls: Arc<ClientConfig>,
}

impl Client {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pool: Rc::new(RefCell::new(Pool::new())),
            tls: tls_config(),
        }
    }

    pub async fn get(&self, url: &str) -> Result<Response, Error> {
        self.get_with(url, &[]).await
    }

    pub async fn get_with(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, Error> {
        let uri = parse_url(url)?;
        self.execute(Method::GET, uri, headers, None, None).await
    }

    pub async fn head(&self, url: &str) -> Result<Response, Error> {
        self.head_with(url, &[]).await
    }

    /// The body is always empty; the response head is enough for existence
    /// probes.
    pub async fn head_with(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, Error> {
        let uri = parse_url(url)?;
        self.execute(Method::HEAD, uri, headers, None, None).await
    }

    pub async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Response, Error> {
        let uri = parse_url(url)?;
        self.execute(Method::POST, uri, headers, Some(content_type), Some(body))
            .await
    }

    async fn execute(
        &self,
        method: Method,
        mut uri: Uri,
        headers: &[(&str, &str)],
        content_type: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<Response, Error> {
        for _ in 0..=MAX_REDIRECTS {
            let key = pool_key(&uri)?;
            let request =
                build_request(&uri, &method, headers, content_type, body.map(<[u8]>::len));

            // A pooled connection can have been closed by the server while it sat
            // idle; HTTP/1.1 only discovers that when the next request is written
            // or read. That is not a failure of the request, so drop the socket
            // and retry once on a fresh connection. A second failure is returned:
            // the request itself is then suspect. Replaying is safe because the
            // only non-GET this client sends is a read-only GQL query.
            let (conn, head) = loop {
                let pooled = self.pool.borrow_mut().take(&key);
                let reused = pooled.is_some();
                let mut conn = match pooled {
                    Some(conn) => conn,
                    None => Conn::connect(&key, &self.tls).await?,
                };

                match send_request(&mut conn, &request, body).await {
                    Ok(head) => break (conn, head),
                    Err(error) if reused && error.stale_connection() => {
                        debug!(%error, host = key.host.as_str(), "stale pooled connection");
                    }
                    Err(error) => return Err(error),
                }
            };

            if head.status.is_redirection()
                && matches!(method, Method::GET | Method::HEAD)
                && let Some(location) = head.headers.get(LOCATION)
            {
                uri = resolve(&uri, location)?;
                continue;
            }

            let mode = body_mode(head.status, &head.headers, &method);
            let reusable = !matches!(mode, Mode::Eof) && keep_alive(head.version, &head.headers);
            let lease = Lease {
                pool: Rc::clone(&self.pool),
                key,
            };

            return Ok(Response {
                status: head.status,
                headers: head.headers,
                body: Body::new(conn, mode, head.leftover, Some(lease), reusable),
            });
        }

        Err(Error::Redirects)
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// A response whose body streams from the connection.
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Body,
}

struct Head {
    status: StatusCode,
    headers: HeaderMap,
    leftover: Vec<u8>,
    version: u8,
}

async fn send_request(conn: &mut Conn, request: &str, body: Option<&[u8]>) -> Result<Head, Error> {
    conn.write_all(request.as_bytes()).await?;
    if let Some(body) = body
        && !body.is_empty()
    {
        conn.write_all(body).await?;
    }
    read_head(conn).await
}

async fn read_head(conn: &mut Conn) -> Result<Head, Error> {
    let mut buffer = Vec::with_capacity(8 * 1024);
    let mut scratch = [0u8; 8 * 1024];

    loop {
        if let Some((status, headers, head_len, version)) = parse_head(&buffer)? {
            let leftover = buffer.split_off(head_len);
            return Ok(Head {
                status,
                headers,
                leftover,
                version,
            });
        }

        let read = conn.read_into(&mut scratch).await?;
        if read == 0 {
            return Err(Error::Closed);
        }
        buffer.extend_from_slice(&scratch[..read]);
        if buffer.len() > MAX_HEAD {
            return Err(Error::Protocol("response head too large"));
        }
    }
}

fn parse_head(buffer: &[u8]) -> Result<Option<(StatusCode, HeaderMap, usize, u8)>, Error> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut response = httparse::Response::new(&mut headers);

    match response.parse(buffer) {
        Ok(httparse::Status::Complete(head_len)) => {
            let code = response.code.ok_or(Error::Protocol("missing status"))?;
            let status =
                StatusCode::from_u16(code).map_err(|_| Error::Protocol("invalid status"))?;

            let mut map = HeaderMap::with_capacity(response.headers.len());
            for header in response.headers.iter().filter(|h| !h.name.is_empty()) {
                let name = http::HeaderName::from_bytes(header.name.as_bytes())
                    .map_err(|_| Error::Protocol("invalid header name"))?;
                let value = http::HeaderValue::from_bytes(header.value)
                    .map_err(|_| Error::Protocol("invalid header value"))?;
                map.append(name, value);
            }

            let version = response.version.unwrap_or(1).min(1);
            Ok(Some((status, map, head_len, version)))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(_) => Err(Error::Protocol("invalid response head")),
    }
}

fn body_mode(status: StatusCode, headers: &HeaderMap, method: &Method) -> Mode {
    if method == Method::HEAD
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
    {
        return Mode::Length(0);
    }

    if headers
        .get(TRANSFER_ENCODING)
        .is_some_and(|value| contains_token(value.as_bytes(), b"chunked"))
    {
        return Mode::Chunked;
    }

    if let Some(value) = headers.get(CONTENT_LENGTH) {
        if let Ok(text) = value.to_str()
            && let Ok(length) = text.trim().parse::<u64>()
        {
            return Mode::Length(length);
        }
        return Mode::Eof;
    }

    Mode::Eof
}

fn keep_alive(version: u8, headers: &HeaderMap) -> bool {
    if let Some(value) = headers.get(CONNECTION) {
        if contains_token(value.as_bytes(), b"close") {
            return false;
        }
        if contains_token(value.as_bytes(), b"keep-alive") {
            return true;
        }
    }
    version == 1
}

fn contains_token(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn parse_url(url: &str) -> Result<Uri, Error> {
    url.parse::<Uri>().map_err(|_| Error::Url(url.to_owned()))
}

fn pool_key(uri: &Uri) -> Result<PoolKey, Error> {
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| Error::Url(uri.to_string()))?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        other => return Err(Error::Scheme(other.to_owned())),
    };
    let authority = uri.authority().ok_or_else(|| Error::Url(uri.to_string()))?;
    let port = authority.port_u16().unwrap_or(if tls { 443 } else { 80 });
    Ok(PoolKey::new(tls, authority.host().to_owned(), port))
}

fn build_request(
    uri: &Uri,
    method: &Method,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body_len: Option<usize>,
) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(method.as_str());
    out.push(' ');
    out.push_str(
        uri.path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str),
    );
    out.push_str(" HTTP/1.1\r\nHost: ");
    out.push_str(uri.authority().map_or("", http::uri::Authority::as_str));
    out.push_str("\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n");

    for (name, value) in headers {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }

    if let Some(content_type) = content_type {
        out.push_str("Content-Type: ");
        out.push_str(content_type);
        out.push_str("\r\n");
    }

    if let Some(len) = body_len {
        out.push_str("Content-Length: ");
        out.push_str(&len.to_string());
        out.push_str("\r\n");
    }

    out.push_str("\r\n");
    out
}

fn resolve(base: &Uri, location: &http::HeaderValue) -> Result<Uri, Error> {
    let location = location
        .to_str()
        .map_err(|_| Error::Protocol("non-ascii location"))?;

    if location.starts_with("http://") || location.starts_with("https://") {
        return parse_url(location);
    }

    let authority = base
        .authority()
        .ok_or(Error::Protocol("redirect without authority"))?;
    let scheme = base.scheme_str().unwrap_or("http");
    let path = if location.starts_with('/') {
        location.to_owned()
    } else {
        format!("/{location}")
    };
    parse_url(&format!("{scheme}://{authority}{path}"))
}

fn tls_config() -> Arc<ClientConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::net::TcpListener;
    use std::net::TcpStream;

    use super::*;

    struct Step {
        pieces: Vec<Vec<u8>>,
        close: bool,
    }

    fn step(response: &str) -> Step {
        Step {
            pieces: vec![response.as_bytes().to_vec()],
            close: false,
        }
    }

    /// Serve `script` on loopback: one request per step, on one connection until a
    /// step asks to close or a new connection is needed.
    fn serve(script: Vec<Step>) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let mut current: Option<TcpStream> = None;
            let mut buffer = [0u8; 4096];

            for step in script {
                if current.is_none() {
                    let (stream, _) = listener.accept().expect("accept");
                    stream.set_nodelay(true).expect("nodelay");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .expect("timeout");
                    current = Some(stream);
                }

                {
                    let stream = current.as_mut().expect("connected");
                    let mut head = Vec::new();
                    loop {
                        let read = stream.read(&mut buffer).expect("read request");
                        if read == 0 {
                            return;
                        }
                        head.extend_from_slice(&buffer[..read]);
                        if head.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    for piece in step.pieces {
                        stream.write_all(&piece).expect("write");
                        stream.flush().expect("flush");
                    }
                }

                if step.close {
                    current = None;
                }
            }
        });
        (addr, handle)
    }

    fn run<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn reads_content_length_body() {
        let (addr, server) = serve(vec![step(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
        )]);
        let client = Client::new();
        let url = format!("http://{addr}/");

        let (status, body) = run(async {
            let mut response = client.get(&url).await.expect("get");
            let status = response.status;
            let body = response.body.read_to_end(1024).await.expect("body");
            (status, body)
        });

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"hello");
        server.join().expect("server");
    }

    #[test]
    fn reads_chunked_body_with_tiny_chunks() {
        let (addr, server) = serve(vec![step(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
             3\r\nabc\r\n5\r\n12345\r\n0\r\n\r\n",
        )]);
        let client = Client::new();
        let url = format!("http://{addr}/");

        let body = run(async {
            client
                .get(&url)
                .await
                .expect("get")
                .body
                .read_to_end(1024)
                .await
                .expect("body")
        });

        assert_eq!(body, b"abc12345");
        server.join().expect("server");
    }

    #[test]
    fn reads_chunked_body_split_across_writes() {
        let mut response = Step {
            pieces: Vec::new(),
            close: false,
        };
        for piece in [
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n",
            "\r\n",
            "3\r\n",
            "abc",
            "\r\n",
            "7\r\n",
            "defghij",
            "\r\n0\r\n",
            "\r\n",
        ] {
            response.pieces.push(piece.as_bytes().to_vec());
        }
        let (addr, server) = serve(vec![response]);
        let client = Client::new();
        let url = format!("http://{addr}/");

        let body = run(async {
            client
                .get(&url)
                .await
                .expect("get")
                .body
                .read_to_end(1024)
                .await
                .expect("body")
        });

        assert_eq!(body, b"abcdefghij");
        server.join().expect("server");
    }

    #[test]
    fn reads_eof_delimited_body() {
        let (addr, server) = serve(vec![Step {
            pieces: vec![b"HTTP/1.0 200 OK\r\n\r\nbody".to_vec()],
            close: true,
        }]);
        let client = Client::new();
        let url = format!("http://{addr}/");

        let body = run(async {
            client
                .get(&url)
                .await
                .expect("get")
                .body
                .read_to_end(1024)
                .await
                .expect("body")
        });

        assert_eq!(body, b"body");
        server.join().expect("server");
    }

    #[test]
    fn reuses_keep_alive_connections() {
        let (addr, server) = serve(vec![
            step("HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na"),
            step("HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nb"),
        ]);
        let client = Client::new();
        let url = format!("http://{addr}/");

        let (first, second) = run(async {
            let first = client
                .get(&url)
                .await
                .expect("first")
                .body
                .read_to_end(16)
                .await
                .expect("body");
            let second = client
                .get(&url)
                .await
                .expect("second")
                .body
                .read_to_end(16)
                .await
                .expect("body");
            (first, second)
        });

        assert_eq!(first, b"a");
        assert_eq!(second, b"b");
        server.join().expect("server");
    }

    #[test]
    fn retries_when_a_pooled_connection_was_closed() {
        let (addr, server) = serve(vec![
            // The first response is well-formed and keep-alive by its headers,
            // but the server drops the socket right after it: the client has no
            // way to know until it reuses the connection.
            Step {
                pieces: vec![b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na".to_vec()],
                close: true,
            },
            step("HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nb"),
        ]);
        let client = Client::new();
        let url = format!("http://{addr}/");

        let (first, second) = run(async {
            let first = client
                .get(&url)
                .await
                .expect("first")
                .body
                .read_to_end(16)
                .await
                .expect("body");
            let second = client
                .get(&url)
                .await
                .expect("second")
                .body
                .read_to_end(16)
                .await
                .expect("body");
            (first, second)
        });

        assert_eq!(first, b"a");
        assert_eq!(second, b"b");
        server.join().expect("server");
    }

    #[test]
    fn follows_redirects() {
        let (addr, server) = serve(vec![
            Step {
                pieces: vec![
                    b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n".to_vec(),
                ],
                close: true,
            },
            step("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndone"),
        ]);
        let client = Client::new();
        let url = format!("http://{addr}/start");

        let body = run(async {
            client
                .get(&url)
                .await
                .expect("get")
                .body
                .read_to_end(64)
                .await
                .expect("body")
        });

        assert_eq!(body, b"done");
        server.join().expect("server");
    }
}

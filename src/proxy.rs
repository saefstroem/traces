use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::handshake as h1_handshake;
use hyper::client::conn::http2::{Builder as H2ClientBuilder, SendRequest as H2Sender};
use hyper::header::UPGRADE;
use hyper::server::conn::http1::Builder as H1ServerBuilder;
use hyper::server::conn::http2::Builder as H2ServerBuilder;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use rustls::server::Acceptor;
use serde_json::{Value, from_slice};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::spawn;
use tokio::sync::Mutex;
use tokio_rustls::client::TlsStream as ClientTls;
use tokio_rustls::server::StartHandshake;
use tokio_rustls::{LazyConfigAcceptor, TlsConnector};
use tracing::{error, info, warn};

use crate::certs::Ca;
use crate::config::Config;
use crate::error::Error;
use crate::providers::Capture;
use crate::resolver::Resolver;
use crate::result::Result;
use crate::store::Store;

type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Represents a proxy that handles incoming connections and forwards them to upstream servers.
pub struct Proxy {
    /// The configuration for traces
    config: Config,
    /// The certificate authority used for TLS connections to upstream servers.
    ca: Ca,
    /// The TLS connector used for HTTP/1 upstream connections.
    upstream_h1: TlsConnector,
    /// The TLS connector used for HTTP/2 upstream connections.
    upstream_h2: TlsConnector,
    /// The dns resolver used to resolve hostnames to IP addresses.
    resolver: Resolver,
    /// The store used to persist captured data.
    store: Arc<Store>,
}

/// Represents an upstream connection, either HTTP/1 or HTTP/2.
enum Upstream {
    Http1(Mutex<Option<Box<ClientTls<TcpStream>>>>),
    Http2(Mutex<H2Sender<ProxyBody>>),
}

impl Proxy {
    /// Creates a new proxy instance with the given configuration, certificate authority, TLS client configuration, and store.
    pub fn new(config: Config, ca: Ca, mut client: ClientConfig, store: Arc<Store>) -> Self {
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        // Create the TLS connector for HTTP/1 upstream connections.
        let upstream_h1 = TlsConnector::from(Arc::new(client.clone()));
        client.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        // Create the TLS connector for HTTP/2 upstream connections.
        let upstream_h2 = TlsConnector::from(Arc::new(client));
        Self { config, ca, upstream_h1, upstream_h2, resolver: Resolver::default(), store }
    }

    pub async fn tunnel(self: Arc<Self>, mut client: TcpStream) -> Result<()> {
        // Read the initial bytes from the client to determine the target host and port.
        let mut head = [0u8; 4096];
        // Read the initial bytes from the client to determine the target host and port.
        let read = client.read(&mut head).await?;
        // Convert the read bytes into a string for easier parsing.
        let head = String::from_utf8_lossy(head.get(..read).unwrap_or_default());
        // Extract the target host and port from the CONNECT request line.
        let target = head.strip_prefix("CONNECT ").and_then(|rest| rest.split_whitespace().next());
        // Split the target into host and port components.
        let (host, port) = target.and_then(|target| target.rsplit_once(':')).ok_or(Error::NotConnect)?;
        let port = port.parse().map_err(|_| Error::NotConnect)?;
        let server_name = ServerName::try_from(host.to_string())?;
        let host = host.to_string();
        // Send the HTTP/1.1 200 Connection Established response to the client.
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        // Begin the TLS handshake with the client using the lazy config acceptor.
        let start = LazyConfigAcceptor::new(Acceptor::default(), client).await?;
        // Accept the TLS handshake and serve the connection.
        self.accept_and_serve(start, host, port, server_name).await
    }

    /// Handles requests to the proxy's own hosts, ensuring they are loopback and not conflicting with the proxy itself.
    pub async fn hosts(self: Arc<Self>, client: TcpStream, port: u16) -> Result<()> {
        // Determine the port on which the proxy itself is listening.
        let own_port = client.local_addr()?.port();
        // Ensure the client is connecting from a loopback address.
        if !client.peer_addr()?.ip().is_loopback() {
            return Err(Error::NotLoopback);
        }
        // Begin the TLS handshake with the client using the lazy config acceptor.
        let start = LazyConfigAcceptor::new(Acceptor::default(), client).await?;
        // Extract the server name from the client's TLS ClientHello.
        let host = start.client_hello().server_name().ok_or(Error::NoServerName)?.to_string();
        let server_name = ServerName::try_from(host.clone())?;
        // Resolve the host to its IP addresses.
        let addresses = self.resolver.addresses(&host, port).await?;
        // Ensure none of the resolved addresses conflict with the proxy's own loopback port.
        if addresses.iter().any(|address| address.ip().is_loopback() && address.port() == own_port) {
            return Err(Error::Loop(host));
        }
        // Accept the TLS handshake and serve the connection.
        self.accept_and_serve(start, host, port, server_name).await
    }

    /// Accepts the TLS handshake and serves the connection,
    /// determining the appropriate upstream protocol (HTTP/1.1 or HTTP/2).
    async fn accept_and_serve(
        self: Arc<Self>,
        start: StartHandshake<TcpStream>,
        host: String,
        port: u16,
        server_name: ServerName<'static>,
    ) -> Result<()> {
        // Determine if the client wants to use HTTP/2 based on the ALPN protocols advertised in the ClientHello.
        let wants_h2 = start.client_hello().alpn().is_some_and(|mut protocols| protocols.any(|proto| proto == b"h2"));

        // Establish the upstream connection based on the desired protocol (HTTP/2 or HTTP/1.1).
        let (alpn, upstream) = if wants_h2 {
            // Connect to the upstream server over TCP.
            let tcp = self.resolver.connect(&host, port).await?;
            // Perform the TLS handshake with the upstream server for HTTP/2.
            let tls = self.upstream_h2.connect(server_name.clone(), tcp).await?;
            // Check the negotiated ALPN protocol to determine if HTTP/2 was successfully negotiated.
            if tls.get_ref().1.alpn_protocol() == Some(b"h2") {
                (b"h2".as_slice(), Upstream::Http2(Mutex::new(h2_handshake(tls).await?)))
            } else {
                // If HTTP/2 was not negotiated, fall back to HTTP/1.1.
                (b"http/1.1".as_slice(), Upstream::Http1(Mutex::new(Some(Box::new(tls)))))
            }
        } else {
            // If the client does not want HTTP/2, default to HTTP/1.1.
            (b"http/1.1".as_slice(), Upstream::Http1(Mutex::new(None)))
        };
        // Perform the TLS handshake with the client using the determined ALPN protocol.
        let client = start.into_stream(Arc::new(self.ca.server_config(&host, alpn)?)).await?;
        // Determine if the upstream connection is using HTTP/2.
        let is_h2 = matches!(upstream, Upstream::Http2(_));
        // Wrap the upstream connection in an Arc for shared ownership across async tasks.
        let upstream = Arc::new(upstream);
        // Create the forward service that will handle incoming requests and forward them to the upstream server.
        let forward = service_fn(move |request: Request<Incoming>| {
            let (proxy, upstream) = (self.clone(), upstream.clone());
            let (host, server_name) = (host.clone(), server_name.clone());
            async move {
                let exchange = async {
                    // Split the incoming request into its parts and body for further processing.
                    let (mut parts, body) = request.into_parts();
                    // Determine the appropriate provider for the request based on its path.
                    let provider = proxy.config.route(parts.uri.path());
                    // If the request is attempting to upgrade the connection and a provider is found, refuse the upgrade.
                    if provider.is_some() && parts.headers.contains_key(UPGRADE) {
                        let mut refusal = Response::new(Empty::new().map_err(|never| match never {}).boxed());
                        *refusal.status_mut() = StatusCode::UPGRADE_REQUIRED;
                        // traces doesnt support connection upgrades (websocket for now), so we return an UPGRADE_REQUIRED response.
                        return Ok(refusal);
                    }
                    let (body, capture) = match provider {
                        // If a provider is found, parse the request body and capture relevant information for tracing.
                        Some(provider) => {
                            // Collect the request body into bytes for parsing.
                            let bytes = body.collect().await?.to_bytes();
                            // Parse the collected bytes as JSON for further processing.
                            let parsed = from_slice::<Value>(&bytes).unwrap_or_default();
                            // Generate a conversation key for tracing based on the provider, request path, headers, and parsed body.
                            let prefix =
                                proxy.config.prefix.key(provider.name(), parts.uri.path(), &parts.headers, &parsed);
                            // Capture the LLM trace
                            let capture = Capture::request(provider, &mut parts, &parsed);
                            // Reconstruct the request body for forwarding to the upstream server.
                            let body = Full::new(bytes).map_err(|never| match never {}).boxed();
                            (body, Some((provider.name(), prefix, capture)))
                        }
                        None => (body.boxed(), None),
                    };
                    // Construct the final request to be sent to the upstream server.
                    let request = Request::from_parts(parts, body);
                    // Send the request to the appropriate upstream server based on the connection type.
                    let response = match &*upstream {
                        // Handle the case where the upstream connection is using HTTP/1.1.
                        Upstream::Http1(first) => {
                            // Attempt to take an existing TLS connection from the cache, or establish a new one if none exists.
                            let tls = match first.lock().await.take() {
                                Some(tls) => *tls,
                                None => {
                                    // Establish a new TCP connection to the upstream server.
                                    let tcp = proxy.resolver.connect(&host, port).await?;
                                    proxy.upstream_h1.connect(server_name, tcp).await?
                                }
                            };
                            // Perform the HTTP/1.1 handshake over the established TLS connection.
                            let (mut sender, connection) = h1_handshake(TokioIo::new(tls)).await?;
                            // Drive the HTTP/1.1 connection in the background.
                            drive(connection);
                            // Send the HTTP/1.1 request using the sender.
                            sender.send_request(request).await?
                        }
                        Upstream::Http2(cached) => {
                            // Handle the case where the upstream connection is using HTTP/2.
                            let mut sender = cached.lock().await.clone();
                            if sender.is_closed() || sender.ready().await.is_err() {
                                // If the cached sender is closed or not ready, establish a new HTTP/2 connection.
                                let mut guard = cached.lock().await;
                                let tcp = proxy.resolver.connect(&host, port).await?;
                                // Establish a new TCP connection to the upstream server.
                                // Perform the HTTP/2 handshake over the established TCP connection.
                                sender = h2_handshake(proxy.upstream_h2.connect(server_name, tcp).await?).await?;
                                *guard = sender.clone();
                                drop(guard);
                                // Ensure the new sender is ready to send requests.
                                sender.ready().await?;
                            }
                            // Send the HTTP/2 request using the sender.
                            sender.send_request(request).await?
                        }
                    };
                    // Map the response body to include any captured traces before returning it.
                    Ok::<_, Error>(response.map(|body| match capture {
                        Some((name, prefix, capture)) => {
                            let store = proxy.store.clone();
                            // Capture the response body and associate it with the corresponding exchange.
                            capture.response(body, move |exchange| {
                                info!("[{name}]\n{exchange}");
                                if let Some(prefix) = prefix {
                                    // If a conversation is present, add the captured traces to the store.
                                    store.add(prefix, exchange.traces);
                                }
                            })
                        }
                        None => body.boxed(),
                    }))
                };
                exchange.await.inspect_err(|error| error!("forward to {host}:{port}: {error}"))
            }
        });
        // Determine whether to use HTTP/2 or HTTP/1 for the server connection based on the `is_h2` flag.
        if is_h2 {
            // Use HTTP/2 server builder for the connection.
            H2ServerBuilder::new(TokioExecutor::new())
                .adaptive_window(true)
                .serve_connection(TokioIo::new(client), forward)
                .await?;
        } else {
            // Use HTTP/1 server builder for the connection.
            H1ServerBuilder::new().serve_connection(TokioIo::new(client), forward).await?;
        }
        Ok(())
    }
}

/// Perform an HTTP/2 handshake over the given TLS connection and return the sender.
async fn h2_handshake(tls: ClientTls<TcpStream>) -> Result<H2Sender<ProxyBody>> {
    let (sender, connection) =
        H2ClientBuilder::new(TokioExecutor::new()).adaptive_window(true).handshake(TokioIo::new(tls)).await?;
    drive(connection);
    Ok(sender)
}

/// Drive the given HTTP connection future to completion, logging any errors.
fn drive<F: Future<Output = hyper::Result<()>> + Send + 'static>(connection: F) {
    spawn(async move {
        if let Err(error) = connection.await {
            warn!("upstream connection error: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use core::pin::Pin;
    use core::result;
    use core::task::{Context, Poll};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Body, Frame, Incoming};
    use hyper::client::conn::http1::{SendRequest as H1Sender, handshake as h1_handshake};
    use hyper::client::conn::http2::Builder as H2ClientBuilder;
    use hyper::server::conn::http1::Builder as H1ServerBuilder;
    use hyper::server::conn::http2::Builder as H2ServerBuilder;
    use hyper::service::service_fn;
    use hyper::{HeaderMap, Request, Response, Version};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::{ClientConfig, RootCertStore};
    use serde_json::{Value, from_slice, json};
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::spawn;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use tokio::task::JoinHandle;
    use tokio::time::sleep;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::Proxy;
    use crate::certs::Ca;
    use crate::config::Config;
    use crate::error::Error;
    use crate::result::Result;
    use crate::store::{Segment, Store};

    const ANTHROPIC_STREAM: &[u8] = include_bytes!("../fixtures/anthropic.sse");
    const GEMINI_STREAM: &[u8] = include_bytes!("../fixtures/gemini.sse");

    struct ChannelBody(UnboundedReceiver<Frame<Bytes>>);

    impl Body for ChannelBody {
        type Data = Bytes;
        type Error = hyper::Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<result::Result<Frame<Bytes>, Self::Error>>> {
            self.0.poll_recv(cx).map(|frame| frame.map(Ok))
        }
    }

    fn proxy(dir: &Path) -> (Arc<Proxy>, TlsConnector, TlsConnector) {
        let ca = Ca::load_or_create(&dir.join("ca")).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from_pem_file(&ca.cert_path).unwrap()).unwrap();
        let client = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        let connector = |alpn: &[&[u8]]| {
            let mut config = client.clone();
            config.alpn_protocols = alpn.iter().map(|proto| proto.to_vec()).collect();
            TlsConnector::from(Arc::new(config))
        };
        let (h1, h2) = (connector(&[b"http/1.1"]), connector(&[b"h2", b"http/1.1"]));
        (Arc::new(Proxy::new(Config::load().unwrap(), ca, client, Arc::new(Store::new(1000)))), h1, h2)
    }

    async fn fake_h1_provider(proxy: &Proxy) -> u16 {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        let acceptor = TlsAcceptor::from(Arc::new(proxy.ca.server_config("localhost", b"http/1.1").unwrap()));
        spawn(async move {
            let tls = acceptor.accept(upstream.accept().await.unwrap().0).await.unwrap();
            let answer = service_fn(|request: Request<Incoming>| async move {
                let encoding = request.headers().get("accept-encoding").cloned();
                let echoed = Response::builder().header("x-seen-encoding", format!("{encoding:?}"));
                Ok::<_, hyper::Error>(echoed.body(Full::new(Bytes::from_static(ANTHROPIC_STREAM))).unwrap())
            });
            let _ = H1ServerBuilder::new().serve_connection(TokioIo::new(tls), answer).await;
        });
        port
    }

    async fn connect(proxy: &Arc<Proxy>, request: &str) -> (TcpStream, JoinHandle<Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let tunnel = spawn(proxy.clone().tunnel(socket));
        client.write_all(request.as_bytes()).await.unwrap();
        (client, tunnel)
    }

    async fn h1_client(proxy: &Arc<Proxy>, trusting: &TlsConnector, port: u16) -> H1Sender<Full<Bytes>> {
        let (mut tcp, _) = connect(proxy, &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n")).await;
        let mut established = [0u8; 39];
        tcp.read_exact(&mut established).await.unwrap();
        assert_eq!(&established, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        let tls = trusting.connect(ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
        let (sender, connection) = h1_handshake(TokioIo::new(tls)).await.unwrap();
        spawn(connection);
        sender
    }

    async fn stored(proxy: &Proxy) -> (Segment, Value) {
        for _ in 0..100 {
            if let Some(segment) = proxy.store.take().pop() {
                let traces = from_slice(&[b"[".as_slice(), &segment.items, b"]"].concat()).unwrap();
                return (segment, traces);
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("nothing was stored");
    }

    #[tokio::test]
    async fn streams_a_provider_response_through_the_tunnel_and_stores_it_under_its_prefix() {
        let dir = tempdir().unwrap();
        let (proxy, h1, _) = proxy(dir.path());
        let port = fake_h1_provider(&proxy).await;
        let mut sender = h1_client(&proxy, &h1, port).await;
        let request = Request::post("/v1/messages?beta=true")
            .header("host", format!("localhost:{port}"))
            .header("accept-encoding", "gzip, br")
            .header("x-claude-code-session-id", "session-1")
            .body(Full::new(Bytes::from_static(
                br#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hello"}]}"#,
            )))
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.headers()["x-seen-encoding"], "None");
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), ANTHROPIC_STREAM);
        let (segment, traces) = stored(&proxy).await;
        assert_eq!(segment.prefix, "anthropic/claude-sonnet-4-6/session-1");
        assert_eq!(traces[0], json!({"input": {"role": "user", "content": "hello"}}));
        assert_eq!(traces[3], json!({"tool": {"name": "Bash", "input": r#"{"command": "ls -la"}"#}}));
    }

    #[tokio::test]
    async fn serves_a_host_that_etc_hosts_sends_here_by_the_name_in_the_tls_hello() {
        let dir = tempdir().unwrap();
        let (proxy, _, h2) = proxy(dir.path());
        let port = fake_h1_provider(&proxy).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        spawn(proxy.clone().hosts(listener.accept().await.unwrap().0, port));
        let tls = h2.connect(ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"http/1.1".as_slice()));
        let (mut sender, connection) = h1_handshake(TokioIo::new(tls)).await.unwrap();
        spawn(connection);
        let request = Request::post("/v1/messages")
            .header("host", "localhost")
            .header("x-claude-code-session-id", "session-2")
            .body(Full::new(Bytes::from_static(br#"{"messages":[{"role":"user","content":"via hosts"}]}"#)))
            .unwrap();
        let body = sender.send_request(request).await.unwrap().into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, ANTHROPIC_STREAM);
        let (segment, traces) = stored(&proxy).await;
        assert_eq!(segment.prefix, "anthropic/unknown/session-2");
        assert_eq!(traces[0], json!({"input": {"role": "user", "content": "via hosts"}}));
    }

    #[tokio::test]
    async fn captures_http2_gemini_sse_and_multiplexes_bidirectional_grpc_via_etc_hosts() {
        let dir = tempdir().unwrap();
        let (proxy, _, h2) = proxy(dir.path());
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = accepts.clone();
        let acceptor = TlsAcceptor::from(Arc::new(proxy.ca.server_config("localhost", b"h2").unwrap()));
        spawn(async move {
            while let Ok((tcp, _)) = upstream.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let acceptor = acceptor.clone();
                spawn(async move {
                    let tls = acceptor.accept(tcp).await.unwrap();
                    let answer = service_fn(|request: Request<Incoming>| async move {
                        assert_eq!(request.version(), Version::HTTP_2);
                        if request.uri().path().ends_with(":streamGenerateContent") {
                            assert!(request.headers().get("accept-encoding").is_none());
                            let body =
                                Full::new(Bytes::from_static(GEMINI_STREAM)).map_err(|never| match never {}).boxed();
                            return Ok::<_, hyper::Error>(Response::new(body));
                        }
                        assert_eq!(request.headers()["content-type"], "application/grpc");
                        assert_eq!(request.headers()["te"], "trailers");
                        let (tx, rx) = unbounded_channel();
                        let mut incoming = request.into_body();
                        spawn(async move {
                            while let Some(Ok(frame)) = incoming.frame().await {
                                if let Some(data) = frame.data_ref().filter(|data| !data.is_empty()) {
                                    let _ = tx.send(Frame::data(Bytes::from([b"echo:".as_slice(), data].concat())));
                                }
                            }
                            let mut trailers = HeaderMap::new();
                            trailers.insert("grpc-status", "0".parse().unwrap());
                            trailers.insert("grpc-message", "ok".parse().unwrap());
                            let _ = tx.send(Frame::trailers(trailers));
                        });
                        let response = Response::builder()
                            .header("content-type", "application/grpc")
                            .body(ChannelBody(rx).boxed())
                            .unwrap();
                        Ok::<_, hyper::Error>(response)
                    });
                    let _ =
                        H2ServerBuilder::new(TokioExecutor::new()).serve_connection(TokioIo::new(tls), answer).await;
                });
            }
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        spawn(proxy.clone().hosts(listener.accept().await.unwrap().0, port));
        let tls = h2.connect(ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
        let (mut sender, connection) =
            H2ClientBuilder::new(TokioExecutor::new()).handshake(TokioIo::new(tls)).await.unwrap();
        spawn(connection);

        let (grpc_tx, grpc_rx) = unbounded_channel();
        let grpc_req = Request::post("https://localhost/google.internal.Test/BidiChat")
            .version(Version::HTTP_2)
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(ChannelBody(grpc_rx).boxed())
            .unwrap();
        sender.ready().await.unwrap();
        grpc_tx.send(Frame::data(Bytes::from_static(b"\x00\x00\x00\x00\x03one"))).unwrap();
        let mut grpc_resp = sender.send_request(grpc_req).await.unwrap().into_body();
        let first = grpc_resp.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first.as_ref(), b"echo:\x00\x00\x00\x00\x03one");
        grpc_tx.send(Frame::data(Bytes::from_static(b"\x00\x00\x00\x00\x03two"))).unwrap();
        drop(grpc_tx);
        let second = grpc_resp.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(second.as_ref(), b"echo:\x00\x00\x00\x00\x03two");
        let trailers = grpc_resp.frame().await.unwrap().unwrap().into_trailers().unwrap();
        assert_eq!(trailers["grpc-status"], "0");
        assert_eq!(trailers["grpc-message"], "ok");

        let gemini_body =
            br#"{"model":"m","request":{"sessionId":"ag-1","contents":[{"role":"user","parts":[{"text":"hi"}]}]}}"#;
        let gemini_req = Request::post("https://localhost/v1internal:streamGenerateContent?alt=sse")
            .version(Version::HTTP_2)
            .header("accept-encoding", "gzip")
            .body(Full::new(Bytes::from_static(gemini_body)).map_err(|never| match never {}).boxed())
            .unwrap();
        sender.ready().await.unwrap();
        let gemini_resp = sender.send_request(gemini_req).await.unwrap();
        assert_eq!(gemini_resp.into_body().collect().await.unwrap().to_bytes(), GEMINI_STREAM);
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
        let (segment, traces) = stored(&proxy).await;
        assert_eq!(segment.prefix, "gemini/m/ag-1");
        assert_eq!(traces[0], json!({"input": {"role": "user", "parts": [{"text": "hi"}]}}));
        assert_eq!(traces[3], json!({"tool": {"name": "run_shell_command", "input": r#"{"command":"ls -la"}"#}}));
    }

    #[tokio::test]
    async fn refuses_a_host_that_points_back_to_its_own_listener() {
        let dir = tempdir().unwrap();
        let (proxy, h1, _) = proxy(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let own_port = listener.local_addr().unwrap().port();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let hosts = spawn(proxy.clone().hosts(listener.accept().await.unwrap().0, own_port));
        let handshake = h1.connect(ServerName::try_from("localhost").unwrap(), tcp).await;
        assert!(matches!(hosts.await.unwrap(), Err(Error::Loop(host)) if host == "localhost"));
        assert!(handshake.is_err());
    }

    #[tokio::test]
    async fn answers_426_to_a_websocket_upgrade_so_the_client_falls_back_to_sse() {
        let dir = tempdir().unwrap();
        let (proxy, h1, _) = proxy(dir.path());
        let mut sender = h1_client(&proxy, &h1, 9).await;
        let request = Request::get("/backend-api/codex/responses")
            .header("host", "localhost:9")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert_eq!(sender.send_request(request).await.unwrap().status(), 426);
    }

    #[tokio::test]
    async fn refuses_a_connect_target_that_is_not_a_server_name() {
        let dir = tempdir().unwrap();
        let (proxy, _, _) = proxy(dir.path());
        let (mut client, tunnel) = connect(&proxy, "CONNECT ../../evil:443 HTTP/1.1\r\n\r\n").await;
        assert!(matches!(tunnel.await.unwrap(), Err(Error::Target(_))));
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).await.unwrap();
        assert!(answer.is_empty());
    }
}

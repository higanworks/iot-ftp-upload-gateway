use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rustls::client::Resumption;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::config::{BackendTlsConfig, BackendTlsMaxVersion};

/// A backend connection that is either plain TCP or TLS over TCP. Lets the control and data
/// paths stay a single code path whether or not `backend_tls` is enabled.
pub enum BackendStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl BackendStream {
    /// The address of the server this connection goes to.
    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        match self {
            BackendStream::Plain(stream) => stream.peer_addr(),
            BackendStream::Tls(stream) => stream.get_ref().0.peer_addr(),
        }
    }
}

impl AsyncRead for BackendStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            BackendStream::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            BackendStream::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for BackendStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            BackendStream::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            BackendStream::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            BackendStream::Plain(stream) => Pin::new(stream).poll_flush(cx),
            BackendStream::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    /// For TLS this sends `close_notify` before shutting down the write half.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            BackendStream::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            BackendStream::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            BackendStream::Plain(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            BackendStream::Tls(stream) => Pin::new(stream.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            BackendStream::Plain(stream) => stream.is_write_vectored(),
            BackendStream::Tls(stream) => stream.is_write_vectored(),
        }
    }
}

/// Builds TLS client connections to the backend. Built once at startup (which also validates the
/// CA file) and cloned into every session, each of which calls `for_session` to get a connector
/// of its own: connections made through the *same* connector share one TLS session-resumption
/// store, which is what lets a data connection resume the control connection's session -- the
/// reuse that backends such as vsftpd (`require_ssl_reuse`) and AWS Transfer Family (by default)
/// demand -- while different sessions never share one.
#[derive(Clone)]
pub struct BackendTlsConnector {
    config: Arc<ClientConfig>,
    connector: TlsConnector,
    server_name: Option<String>,
}

/// How many TLS sessions (TLS 1.3 tickets) a session's resumption store may hold. A session talks
/// to one backend, so it needs few -- but not as few as it seems: rustls sizes the store in
/// groups of 8 tickets per server name and evicts the oldest entry as soon as the store is
/// "full", which for a store of just one group means the entry it has only just inserted. Sized
/// for a single server name (anything up to 8) the store never resumes anything, so keep it at
/// two groups or more; the `..._keeps_resuming_across_many_connections` tests would catch a change
/// that breaks this.
const SESSION_RESUMPTION_ENTRIES: usize = 32;

impl BackendTlsConnector {
    /// Fails if `ca_file` cannot be read, contains no certificates, or holds unusable ones, so a
    /// bad CA file is caught at startup rather than on the first session.
    pub fn new(config: &BackendTlsConfig) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        match &config.ca_file {
            Some(path) => {
                let mut added = 0usize;
                for cert in CertificateDer::pem_file_iter(path).with_context(|| {
                    format!("failed to read backend TLS CA file {}", path.display())
                })? {
                    let cert = cert.with_context(|| {
                        format!("invalid PEM in backend TLS CA file {}", path.display())
                    })?;
                    roots.add(cert).with_context(|| {
                        format!(
                            "unusable certificate in backend TLS CA file {}",
                            path.display()
                        )
                    })?;
                    added += 1;
                }
                if added == 0 {
                    bail!(
                        "backend TLS CA file {} contains no certificates",
                        path.display()
                    );
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }

        let versions: &[&rustls::SupportedProtocolVersion] = match config.max_version {
            BackendTlsMaxVersion::V1_2 => &[&rustls::version::TLS12],
            BackendTlsMaxVersion::V1_3 => &[&rustls::version::TLS13, &rustls::version::TLS12],
        };
        let client_config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(versions)
                .context("failed to select TLS protocol versions")?
                .with_root_certificates(roots)
                .with_no_client_auth();

        let client_config = Arc::new(client_config);
        Ok(BackendTlsConnector {
            connector: TlsConnector::from(Arc::clone(&client_config)),
            config: client_config,
            server_name: config.server_name.clone(),
        })
    }

    /// A connector for one FTP session, with a TLS resumption store of its own.
    ///
    /// Backends that enforce session reuse (RFC 4217; vsftpd's `require_ssl_reuse`) want a data
    /// connection to resume the *control connection's* session. With one store shared by every
    /// session, a data connection could resume a ticket another session's control connection had
    /// left there -- and a new session's control connection would "resume" a stranger's session
    /// -- so the session gets its own: its control and data connections share it, and nothing
    /// else does. It is dropped with the session.
    ///
    /// Everything else (certificate verification, protocol versions, the CA roots) is shared with
    /// `self`; only the small resumption store is new.
    pub fn for_session(&self) -> BackendTlsConnector {
        let mut config = ClientConfig::clone(&self.config);
        config.resumption = Resumption::in_memory_sessions(SESSION_RESUMPTION_ENTRIES);
        let config = Arc::new(config);
        BackendTlsConnector {
            connector: TlsConnector::from(Arc::clone(&config)),
            config,
            server_name: self.server_name.clone(),
        }
    }

    /// Runs the TLS handshake over `tcp`. The certificate is verified against the configured
    /// `server_name`, or `backend_host` when none is set -- never against the peer's IP address,
    /// so a PASV reply's address does not affect verification.
    pub async fn connect(&self, backend_host: &str, tcp: TcpStream) -> io::Result<BackendStream> {
        let name = self.server_name.as_deref().unwrap_or(backend_host);
        let server_name = ServerName::try_from(name.to_owned()).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid TLS server name '{name}': {err}"),
            )
        })?;
        let stream = self.connector.connect(server_name, tcp).await?;
        Ok(BackendStream::Tls(Box::new(stream)))
    }
}

/// Upper bound on the lines of one (possibly multi-line) reply, so a backend can't keep the
/// gateway reading forever during negotiation.
const MAX_REPLY_LINES: usize = 32;

/// Reads one FTP reply (single- or multi-line, RFC 959) and returns its 3-digit code.
async fn read_reply_code<S>(
    conn: &mut BufReader<S>,
    timeout: Duration,
    max_line_bytes: usize,
) -> io::Result<String>
where
    S: AsyncRead + Unpin,
{
    let read = async {
        let mut line = String::new();
        let mut code: Option<String> = None;
        for _ in 0..MAX_REPLY_LINES {
            line.clear();
            let n = (&mut *conn)
                .take(max_line_bytes as u64)
                .read_line(&mut line)
                .await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "backend closed the connection during TLS negotiation",
                ));
            }
            if !line.ends_with('\n') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "backend reply line exceeds maximum allowed length",
                ));
            }
            let bytes = line.as_bytes();
            let well_formed = bytes.len() >= 4 && bytes[..3].iter().all(u8::is_ascii_digit);
            if !well_formed {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed reply from backend during TLS negotiation",
                ));
            }
            match &code {
                None => {
                    if bytes[3] != b'-' {
                        return Ok(line[..3].to_string());
                    }
                    code = Some(line[..3].to_string());
                }
                Some(first) => {
                    if line.starts_with(first.as_str()) && bytes[3] == b' ' {
                        return Ok(first.clone());
                    }
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backend reply has too many lines",
        ))
    };
    tokio::time::timeout(timeout, read).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for backend reply during TLS negotiation",
        )
    })?
}

async fn command_expecting<S>(
    conn: &mut BufReader<S>,
    command: &str,
    expected_code: &str,
    timeout: Duration,
    max_line_bytes: usize,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    conn.write_all(format!("{command}\r\n").as_bytes()).await?;
    conn.flush().await?;
    let code = read_reply_code(conn, timeout, max_line_bytes).await?;
    if code != expected_code {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("backend answered {code} to '{command}', expected {expected_code}"),
        ));
    }
    Ok(())
}

/// Upgrades a freshly connected backend control connection (banner already consumed) to
/// Explicit FTPS: `AUTH TLS` -> handshake -> `PBSZ 0` -> `PROT P`. `PROT P` makes the backend
/// expect TLS on every data connection too.
///
/// Fails closed: any refusal or error is returned to the caller, which must end the session --
/// there is no fallback to plain FTP.
pub async fn negotiate_explicit_ftps(
    mut conn: BufReader<BackendStream>,
    connector: &BackendTlsConnector,
    backend_host: &str,
    command_timeout: Duration,
    handshake_timeout: Duration,
    max_line_bytes: usize,
) -> io::Result<BufReader<BackendStream>> {
    command_expecting(
        &mut conn,
        "AUTH TLS",
        "234",
        command_timeout,
        max_line_bytes,
    )
    .await?;

    // Anything the backend sent after its 234 but before the handshake was sent in plaintext,
    // yet would be read as if it came over the secured channel once `into_inner` drops the
    // buffer (STARTTLS command injection). A well-behaved server never sends any.
    if !conn.buffer().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backend sent data ahead of the TLS handshake",
        ));
    }
    let BackendStream::Plain(tcp) = conn.into_inner() else {
        return Err(io::Error::other("backend connection is already TLS"));
    };

    let tls = tokio::time::timeout(handshake_timeout, connector.connect(backend_host, tcp))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "backend TLS handshake timed out")
        })??;
    let mut conn = BufReader::new(tls);

    command_expecting(&mut conn, "PBSZ 0", "200", command_timeout, max_line_bytes).await?;
    command_expecting(&mut conn, "PROT P", "200", command_timeout, max_line_bytes).await?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    use rustls::HandshakeKind;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_rustls::TlsAcceptor;

    struct TestPki {
        ca_pem: String,
        leaf_der: CertificateDer<'static>,
        leaf_key_der: Vec<u8>,
    }

    fn test_pki(leaf_name: &str) -> TestPki {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec![leaf_name.to_string()])
            .unwrap()
            .signed_by(&leaf_key, &ca, &ca_key)
            .unwrap();

        TestPki {
            ca_pem: ca.pem(),
            leaf_der: leaf.der().clone(),
            leaf_key_der: leaf_key.serialize_der(),
        }
    }

    fn write_temp(contents: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "iot-ftp-gw-tls-test-{}-{}.pem",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn tls_config(ca_pem: &str) -> BackendTlsConfig {
        BackendTlsConfig {
            ca_file: Some(write_temp(ca_pem)),
            ..BackendTlsConfig::default()
        }
    }

    /// Accepts `connections` TLS connections, echoing one byte on each, and reports how each
    /// handshake was performed (full vs. resumed).
    async fn spawn_tls_echo_server(
        pki: &TestPki,
        connections: usize,
    ) -> (std::net::SocketAddr, mpsc::UnboundedReceiver<HandshakeKind>) {
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![pki.leaf_der.clone()],
            PrivateKeyDer::from(PrivatePkcs8KeyDer::from(pki.leaf_key_der.clone())),
        )
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            for _ in 0..connections {
                let (tcp, _) = listener.accept().await.unwrap();
                let Ok(mut stream) = acceptor.accept(tcp).await else {
                    continue;
                };
                if let Some(kind) = stream.get_ref().1.handshake_kind() {
                    let _ = tx.send(kind);
                }
                let mut byte = [0u8; 1];
                if stream.read_exact(&mut byte).await.is_ok() {
                    let _ = stream.write_all(&byte).await;
                    let _ = stream.shutdown().await;
                }
            }
        });
        (addr, rx)
    }

    async fn round_trip(connector: &BackendTlsConnector, addr: std::net::SocketAddr, host: &str) {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut stream = connector.connect(host, tcp).await.unwrap();
        stream.write_all(b"x").await.unwrap();
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"x");
    }

    #[tokio::test]
    async fn connects_and_verifies_against_custom_ca() {
        let pki = test_pki("localhost");
        let (addr, _rx) = spawn_tls_echo_server(&pki, 1).await;
        let connector = BackendTlsConnector::new(&tls_config(&pki.ca_pem)).unwrap();
        round_trip(&connector, addr, "localhost").await;
    }

    #[tokio::test]
    async fn second_connection_resumes_the_first_session() {
        let pki = test_pki("localhost");
        let (addr, mut rx) = spawn_tls_echo_server(&pki, 2).await;
        let connector = BackendTlsConnector::new(&tls_config(&pki.ca_pem)).unwrap();

        // The first round trip also ensures the client has read the server's post-handshake
        // session tickets before the second connection looks for one to resume.
        round_trip(&connector, addr, "localhost").await;
        round_trip(&connector, addr, "localhost").await;

        assert_eq!(rx.recv().await.unwrap(), HandshakeKind::Full);
        assert_eq!(rx.recv().await.unwrap(), HandshakeKind::Resumed);
    }

    #[tokio::test]
    async fn second_connection_resumes_with_tls12_pinned() {
        let pki = test_pki("localhost");
        let (addr, mut rx) = spawn_tls_echo_server(&pki, 2).await;
        let mut config = tls_config(&pki.ca_pem);
        config.max_version = BackendTlsMaxVersion::V1_2;
        let connector = BackendTlsConnector::new(&config).unwrap();

        round_trip(&connector, addr, "localhost").await;
        round_trip(&connector, addr, "localhost").await;

        assert_eq!(rx.recv().await.unwrap(), HandshakeKind::Full);
        assert_eq!(rx.recv().await.unwrap(), HandshakeKind::Resumed);
    }

    #[tokio::test]
    async fn a_new_session_does_not_resume_another_sessions_tls_session() {
        let pki = test_pki("localhost");
        let (addr, mut rx) = spawn_tls_echo_server(&pki, 4).await;
        let base = BackendTlsConnector::new(&tls_config(&pki.ca_pem)).unwrap();
        let session_a = base.for_session();
        let session_b = base.for_session();

        // Control connections of two sessions, then each session's data connection.
        round_trip(&session_a, addr, "localhost").await;
        round_trip(&session_b, addr, "localhost").await;
        round_trip(&session_a, addr, "localhost").await;
        round_trip(&session_b, addr, "localhost").await;

        // Session B's first connection is a full handshake -- it does not pick up the ticket
        // session A left behind -- and each data connection resumes (its own session's ticket).
        let kinds: Vec<_> = vec![
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
        ];
        assert_eq!(
            kinds,
            [
                HandshakeKind::Full,
                HandshakeKind::Full,
                HandshakeKind::Resumed,
                HandshakeKind::Resumed
            ]
        );
    }

    #[tokio::test]
    async fn a_connector_shared_by_two_sessions_would_resume_a_strangers_session() {
        // The behavior `for_session` exists to prevent, shown on the un-split connector: the
        // second "session" resumes the first one's ticket on its very first connection.
        let pki = test_pki("localhost");
        let (addr, mut rx) = spawn_tls_echo_server(&pki, 2).await;
        let shared = BackendTlsConnector::new(&tls_config(&pki.ca_pem)).unwrap();

        round_trip(&shared, addr, "localhost").await; // session A's control connection
        round_trip(&shared, addr, "localhost").await; // session B's control connection

        assert_eq!(rx.recv().await.unwrap(), HandshakeKind::Full);
        assert_eq!(rx.recv().await.unwrap(), HandshakeKind::Resumed);
    }

    /// A TLS 1.3 ticket can be used once, so a session's store has to be topped up by the
    /// tickets the server sends on each connection of that session, or a session making many
    /// uploads would run out and fall back to full handshakes -- which a backend enforcing
    /// session reuse refuses.
    async fn a_session_resumes_on_every_one_of_many_connections(max_version: BackendTlsMaxVersion) {
        const CONNECTIONS: usize = 15;
        let pki = test_pki("localhost");
        let (addr, mut rx) = spawn_tls_echo_server(&pki, CONNECTIONS).await;
        let mut config = tls_config(&pki.ca_pem);
        config.max_version = max_version;
        let session = BackendTlsConnector::new(&config).unwrap().for_session();

        for _ in 0..CONNECTIONS {
            round_trip(&session, addr, "localhost").await;
        }

        assert_eq!(rx.recv().await.unwrap(), HandshakeKind::Full);
        for n in 1..CONNECTIONS {
            assert_eq!(
                rx.recv().await.unwrap(),
                HandshakeKind::Resumed,
                "connection {n} of the session did not resume"
            );
        }
    }

    #[tokio::test]
    async fn a_tls13_session_keeps_resuming_across_many_connections() {
        a_session_resumes_on_every_one_of_many_connections(BackendTlsMaxVersion::V1_3).await;
    }

    #[tokio::test]
    async fn a_tls12_session_keeps_resuming_across_many_connections() {
        a_session_resumes_on_every_one_of_many_connections(BackendTlsMaxVersion::V1_2).await;
    }

    #[tokio::test]
    async fn server_name_override_is_used_for_verification() {
        let pki = test_pki("ftp.example.com");
        let (addr, _rx) = spawn_tls_echo_server(&pki, 1).await;
        let mut config = tls_config(&pki.ca_pem);
        config.server_name = Some("ftp.example.com".to_string());
        let connector = BackendTlsConnector::new(&config).unwrap();
        // The backend is reached as "127.0.0.1", which the certificate does not cover.
        round_trip(&connector, addr, "127.0.0.1").await;
    }

    #[tokio::test]
    async fn rejects_certificate_for_a_different_name() {
        let pki = test_pki("other.example.com");
        let (addr, _rx) = spawn_tls_echo_server(&pki, 1).await;
        let connector = BackendTlsConnector::new(&tls_config(&pki.ca_pem)).unwrap();
        let tcp = TcpStream::connect(addr).await.unwrap();
        assert!(connector.connect("localhost", tcp).await.is_err());
    }

    #[tokio::test]
    async fn rejects_certificate_from_untrusted_ca() {
        let server_pki = test_pki("localhost");
        let other_pki = test_pki("localhost");
        let (addr, _rx) = spawn_tls_echo_server(&server_pki, 1).await;
        let connector = BackendTlsConnector::new(&tls_config(&other_pki.ca_pem)).unwrap();
        let tcp = TcpStream::connect(addr).await.unwrap();
        assert!(connector.connect("localhost", tcp).await.is_err());
    }

    #[test]
    fn rejects_ca_file_without_certificates() {
        let config = BackendTlsConfig {
            ca_file: Some(write_temp("not a certificate\n")),
            ..BackendTlsConfig::default()
        };
        assert!(BackendTlsConnector::new(&config).is_err());
    }

    #[test]
    fn rejects_missing_ca_file() {
        let config = BackendTlsConfig {
            ca_file: Some(PathBuf::from("/nonexistent/ca.pem")),
            ..BackendTlsConfig::default()
        };
        assert!(BackendTlsConnector::new(&config).is_err());
    }

    #[test]
    fn builds_with_bundled_public_roots_when_no_ca_file() {
        assert!(BackendTlsConnector::new(&BackendTlsConfig::default()).is_ok());
    }
}

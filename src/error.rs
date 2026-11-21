use std::error::Error as _;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

use hickory_resolver::net::NetError;
use rustls::pki_types::InvalidDnsNameError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read config.json: {0}")]
    Config(#[from] serde_json::Error),
    #[error("missing environment variables: {0} (see .env.template)")]
    MissingEnv(String),
    #[error("{0} must be a whole number above 0")]
    InvalidEnv(&'static str),
    #[error("HOME is not set, so `~/` in config.json has no meaning")]
    NoHome,
    #[error("file {path}: {source}")]
    File { path: PathBuf, source: io::Error },
    #[error("cannot make a certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("cannot start the async runtime: {0}")]
    Runtime(io::Error),
    #[error("cannot listen on {address}: {source}")]
    Bind { address: SocketAddr, source: io::Error },
    #[error("the first request is not `CONNECT host:port`")]
    NotConnect,
    #[error("the /etc/hosts listener accepts only connections from this computer")]
    NotLoopback,
    #[error("the TLS ClientHello has no server name (SNI)")]
    NoServerName,
    #[error("{0} points back to traces itself")]
    Loop(String),
    #[error("cannot start `{command}`: {source}")]
    Command { command: String, source: io::Error },
    #[error("`{0}` failed")]
    Failed(String),
    #[error("no known CA trust store on this system (see the `system` section of config.json)")]
    NoTrustStore,
    #[error("the root step did not finish (see the messages above)")]
    Elevation,
    #[error("usage: traces [install | start | uninstall]")]
    Usage,
    #[error("run this command without sudo: traces asks sudo itself for the steps that need root")]
    WithoutSudo,
    #[error("{0} contains a character that the service file cannot hold: a line break, <, >, & or \"")]
    Unsafe(&'static str),
    #[error("DNS: {0}")]
    Dns(#[from] NetError),
    #[error("the CONNECT host is not a valid server name: {0}")]
    Target(#[from] InvalidDnsNameError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("HTTP: {0}{cause}", cause = .0.source().map(|cause| format!(": {cause}")).unwrap_or_default())]
    Http(#[from] hyper::Error),
}

use std::net::{IpAddr, SocketAddr};

use hickory_resolver::TokioResolver;
use hickory_resolver::config::ResolveHosts;
use tokio::net::{TcpStream, lookup_host};
use tokio::sync::OnceCell;

use crate::error::Error;
use crate::result::Result;

#[derive(Default)]
/// A DNS resolver that can use the system resolver or a direct DNS query for redirected hosts.
pub struct Resolver {
    dns: OnceCell<TokioResolver>,
}

impl Resolver {
    /// Resolve the addresses for the given host and port, using the system resolver or direct DNS if redirected.
    pub async fn addresses(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        let system: Vec<SocketAddr> = lookup_host((host, port)).await?.collect();
        if redirected(host, &system) { self.dns(host, port).await } else { Ok(system) }
    }

    /// Connect to the given host and port, using the resolved addresses.
    pub async fn connect(&self, host: &str, port: u16) -> Result<TcpStream> {
        Ok(TcpStream::connect(self.addresses(host, port).await?.as_slice()).await?)
    }

    /// Resolve the addresses for the given host and port using direct DNS.
    async fn dns(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        // Initialize the system resolver for direct DNS queries.
        let system = || async {
            // Build the TokioResolver with the specified options for direct DNS queries.
            let mut builder = TokioResolver::builder_tokio()?;
            builder.options_mut().use_hosts_file = ResolveHosts::Never;
            Ok::<_, Error>(builder.build()?)
        };
        // Attempt to get or initialize the DNS resolver for direct queries.
        let dns = self.dns.get_or_try_init(system).await?;
        // Perform the DNS lookup for the host using the direct resolver.
        Ok(dns.lookup_ip(host).await?.iter().map(|ip| SocketAddr::new(ip, port)).collect())
    }
}

/// Determine whether the given host and system-resolved addresses indicate a redirect from /etc/hosts.
fn redirected(host: &str, system: &[SocketAddr]) -> bool {
    let named = !host.eq_ignore_ascii_case("localhost") && host.parse::<IpAddr>().is_err();
    named && system.iter().all(|address| address.ip().is_loopback())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use hickory_resolver::TokioResolver;
    use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolveHosts, ResolverConfig};
    use hickory_resolver::net::runtime::TokioRuntimeProvider;
    use tokio::net::{TcpListener, UdpSocket};
    use tokio::spawn;
    use tokio::sync::OnceCell;

    use super::{Resolver, redirected};

    #[test]
    fn treats_a_loopback_answer_for_a_real_host_name_as_a_redirect_from_etc_hosts() {
        let loopback: SocketAddr = "127.0.0.1:443".parse().unwrap();
        let public: SocketAddr = "203.0.113.7:443".parse().unwrap();
        assert!(redirected("api.anthropic.com", &[loopback]));
        assert!(!redirected("api.anthropic.com", &[public]));
        assert!(!redirected("localhost", &[loopback]));
        assert!(!redirected("127.0.0.1", &[loopback]));
    }

    #[tokio::test]
    async fn asks_dns_directly_for_a_redirected_host_and_connects_to_localhost_directly() {
        let dns = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut connection = ConnectionConfig::udp();
        connection.port = dns.local_addr().unwrap().port();
        spawn(async move {
            let mut query = [0u8; 512];
            loop {
                let (read, client) = dns.recv_from(&mut query).await.unwrap();
                let question_end = 12 + query[12..read].iter().position(|&byte| byte == 0).unwrap() + 5;
                let is_a = query[question_end - 4..question_end - 2] == [0, 1];
                let mut answer = [&query[..2], [0x81, 0x80, 0, 1, 0, u8::from(is_a), 0, 0, 0, 0].as_slice()].concat();
                answer.extend_from_slice(&query[12..question_end]);
                if is_a {
                    answer.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 203, 0, 113, 7]);
                }
                dns.send_to(&answer, client).await.unwrap();
            }
        });
        let server = NameServerConfig::new(IpAddr::V4(Ipv4Addr::LOCALHOST), true, vec![connection]);
        let config = ResolverConfig::from_name_servers(vec![server]);
        let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
        builder.options_mut().use_hosts_file = ResolveHosts::Never;
        let resolver = Resolver { dns: OnceCell::from(builder.build().unwrap()) };
        assert_eq!(resolver.dns("api.example.com", 443).await.unwrap(), ["203.0.113.7:443".parse().unwrap()]);
        let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = local.local_addr().unwrap().port();
        let connected = resolver.connect("localhost", port).await.unwrap();
        assert_eq!(connected.peer_addr().unwrap().port(), port);
    }
}

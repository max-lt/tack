//! Egress proxy for sandboxed commands.
//!
//! The sandbox VM has no route to the internet. Its only way out is this
//! proxy on the host, which tunnels HTTPS (`CONNECT host:443`) to the hosts
//! of an allowlist. The proxy resolves the name itself and refuses
//! addresses that are not public, so an allowed name cannot point back at
//! the host or the LAN. Plain HTTP is refused: one keep-alive connection
//! could carry requests for several hosts.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The only port the proxy tunnels to.
const HTTPS_PORT: u16 = 443;

/// Upper bound for the request head, so a client cannot make the proxy
/// buffer without limit.
const MAX_HEAD: usize = 8 * 1024;

/// Hosts the proxy tunnels to. `*.example.com` matches the subdomains of
/// `example.com`, not `example.com` itself.
#[derive(Debug, Clone)]
pub struct Allowlist {
    patterns: Vec<String>,
}

impl Allowlist {
    pub fn new<S: Into<String>>(patterns: impl IntoIterator<Item = S>) -> Self {
        Self {
            patterns: patterns
                .into_iter()
                .map(|p| p.into().to_ascii_lowercase())
                .collect(),
        }
    }

    pub fn allows(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();

        self.patterns
            .iter()
            .any(|pattern| match pattern.strip_prefix("*.") {
                Some(domain) => host
                    .strip_suffix(domain)
                    .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
                None => *pattern == host,
            })
    }
}

/// The IPv4 network clients must come from.
#[derive(Debug, Clone, Copy)]
pub struct Peers {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Peers {
    fn contains(&self, ip: IpAddr) -> bool {
        let IpAddr::V4(ip) = ip else {
            return false;
        };

        let mask = u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0);
        u32::from(ip) & mask == u32::from(self.network) & mask
    }
}

pub struct EgressProxy {
    pub port: u16,
}

impl EgressProxy {
    /// Listen on every interface on `port` (0 for a free one). Connections
    /// from outside `peers` are closed at once.
    pub async fn start(allowlist: Allowlist, peers: Peers, port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
        let port = listener.local_addr()?.port();

        tokio::spawn(async move {
            loop {
                let Ok((client, peer)) = listener.accept().await else {
                    continue;
                };

                if !peers.contains(peer.ip()) {
                    continue;
                }

                let allowlist = allowlist.clone();
                tokio::spawn(async move {
                    let _ = serve(client, &allowlist).await;
                });
            }
        });

        Ok(Self { port })
    }
}

async fn serve(mut client: TcpStream, allowlist: &Allowlist) -> io::Result<()> {
    let (head, early) = read_head(&mut client).await?;

    let target = match parse_connect(&head) {
        Ok(target) => target,
        Err(status) => return reply(&mut client, status).await,
    };

    let upstream = match open(&target, allowlist).await {
        Ok(upstream) => upstream,
        Err(status) => return reply(&mut client, status).await,
    };

    let mut upstream = TcpStream::connect(upstream).await?;
    upstream.write_all(&early).await?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;

    Ok(())
}

/// Read up to the end of the request head. Returns the head and the bytes
/// the client sent after it, which belong to the tunnel.
async fn read_head(client: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];

    loop {
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let early = data.split_off(end + 4);
            return Ok((data, early));
        }

        if data.len() >= MAX_HEAD {
            return Err(io::Error::other("request head too large"));
        }

        let n = client.read(&mut buf).await?;

        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }

        data.extend_from_slice(&buf[..n]);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
}

/// Status line for a refused request.
type Refusal = &'static str;

const BAD_REQUEST: Refusal = "400 Bad Request";
const FORBIDDEN: Refusal = "403 Forbidden";
const NOT_ALLOWED: Refusal = "405 Method Not Allowed";
const BAD_GATEWAY: Refusal = "502 Bad Gateway";

fn parse_connect(head: &[u8]) -> Result<Target, Refusal> {
    let head = std::str::from_utf8(head).map_err(|_| BAD_REQUEST)?;
    let line = head.lines().next().ok_or(BAD_REQUEST)?;
    let mut parts = line.split(' ');

    let (Some(method), Some(authority), Some(_version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(BAD_REQUEST);
    };

    if method != "CONNECT" {
        return Err(NOT_ALLOWED);
    }

    let (host, port) = authority.rsplit_once(':').ok_or(BAD_REQUEST)?;
    let port = port.parse().map_err(|_| BAD_REQUEST)?;

    Ok(Target {
        host: host.to_string(),
        port,
    })
}

/// Check the target and pick the address to connect to. The address is the
/// one that was checked: nothing resolves the name again.
async fn open(target: &Target, allowlist: &Allowlist) -> Result<SocketAddr, Refusal> {
    // An IP literal (including `[::1]`) would skip the name check.
    let bare = target.host.trim_start_matches('[').trim_end_matches(']');

    if target.port != HTTPS_PORT || bare.parse::<IpAddr>().is_ok() {
        return Err(FORBIDDEN);
    }

    if !allowlist.allows(&target.host) {
        return Err(FORBIDDEN);
    }

    let addrs = tokio::net::lookup_host((target.host.as_str(), target.port))
        .await
        .map_err(|_| BAD_GATEWAY)?
        .collect::<Vec<_>>();

    // Every address must be public: a name that also resolves to a private
    // one is suspect, and the client might retry until it gets that one.
    if addrs.is_empty() || !addrs.iter().all(|a| is_public(a.ip())) {
        return Err(FORBIDDEN);
    }

    Ok(addrs[0])
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(ip),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();

    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        // 100.64.0.0/10, carrier-grade NAT
        || (a == 100 && (64..128).contains(&b))
        // 0.0.0.0/8 and 240.0.0.0/4
        || a == 0
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];

    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7, unique local
        || (first & 0xfe00) == 0xfc00
        // fe80::/10, link local
        || (first & 0xffc0) == 0xfe80)
}

async fn reply(client: &mut TcpStream, status: Refusal) -> io::Result<()> {
    client
        .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_exact_and_subdomains() {
        let allow = Allowlist::new(["crates.io", "*.crates.io"]);

        assert!(allow.allows("crates.io"));
        assert!(allow.allows("static.crates.io"));
        assert!(allow.allows("Static.Crates.IO"));
        assert!(!allow.allows("evilcrates.io"));
        assert!(!allow.allows("crates.io.evil.com"));
        assert!(!Allowlist::new(["*.crates.io"]).allows("crates.io"));
    }

    #[test]
    fn only_connect_is_accepted() {
        assert_eq!(
            parse_connect(b"CONNECT crates.io:443 HTTP/1.1\r\nHost: crates.io\r\n\r\n"),
            Ok(Target {
                host: "crates.io".into(),
                port: 443
            })
        );
        assert_eq!(
            parse_connect(b"GET http://crates.io/ HTTP/1.1\r\n\r\n"),
            Err(NOT_ALLOWED)
        );
        assert_eq!(
            parse_connect(b"CONNECT crates.io HTTP/1.1\r\n\r\n"),
            Err(BAD_REQUEST)
        );
    }

    #[test]
    fn private_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.128.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd34::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }

        assert!(is_public("1.1.1.1".parse().unwrap()));
        assert!(is_public("2606:4700::1111".parse().unwrap()));
    }

    #[test]
    fn peers_contains_its_subnet_only() {
        let peers = Peers {
            network: Ipv4Addr::new(192, 168, 128, 0),
            prefix: 24,
        };

        assert!(peers.contains("192.168.128.7".parse().unwrap()));
        assert!(!peers.contains("192.168.8.7".parse().unwrap()));
        assert!(!peers.contains("::1".parse().unwrap()));
    }

    async fn ask(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut buf = vec![0u8; 256];
        let n = stream.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n])
            .lines()
            .next()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn proxy_refuses_what_it_must() {
        let proxy = EgressProxy::start(
            Allowlist::new(["localhost", "allowed.invalid"]),
            Peers {
                network: Ipv4Addr::LOCALHOST,
                prefix: 8,
            },
            0,
        )
        .await
        .unwrap();

        let refused = [
            ("CONNECT example.com:443 HTTP/1.1\r\n\r\n", "403"),
            ("CONNECT 1.1.1.1:443 HTTP/1.1\r\n\r\n", "403"),
            ("CONNECT [::1]:443 HTTP/1.1\r\n\r\n", "403"),
            ("CONNECT localhost:22 HTTP/1.1\r\n\r\n", "403"),
            // Allowed name, loopback address
            ("CONNECT localhost:443 HTTP/1.1\r\n\r\n", "403"),
            ("GET http://localhost/ HTTP/1.1\r\n\r\n", "405"),
        ];

        for (request, status) in refused {
            let line = ask(proxy.port, request).await;
            assert!(line.contains(status), "{request:?} -> {line}");
        }
    }
}

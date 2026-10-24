use anyhow::{Result, anyhow};
use rand::Rng;
use std::{
    collections::HashSet,
    fmt,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6, Ipv6Addr}
};

use bencodedit::{accessor::BencodeAccessor, BencodeValue};

pub type ClientVersion = [u8; 2];
pub const CLIENT_VERSION: &'static ClientVersion = b"LV"; // libvictoria

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(pub [u8; 20]);

impl Hash {
    pub fn from(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn from_xt(xt: &str) -> Result<Self> {
        let bytes: [u8; 20] = hex::decode(
            xt.strip_prefix("urn:btih:").ok_or(
                anyhow::anyhow!("Only the btih format is supported for xt")
            )?
        )?.try_into().map_err(|_| anyhow::anyhow!("Btih format requires 20 bytes"))?;
        Ok(Self(bytes))
    }

    pub fn encode(&self) -> String {
        self.0
            .iter() .map(|b| format!("%{b:02X}"))
            .collect::<String>()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.map(|b| format!("{b:02x}")).join(""))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id(pub [u8; 20]);
pub type PeerId = Id;

impl PeerId {
    pub fn from(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> [u8; 20] {
        self.0
    }

    pub fn random() -> Self {
        let mut bytes = [0; 20];
        bytes[0..2].copy_from_slice(CLIENT_VERSION);
        rand::rng().fill_bytes(&mut bytes[2..]);
        Self(bytes)
    }

    pub fn to_compact(&self) -> String {
        let id = self.0;
        format!(
            "{:02x}{:02x}{:02x}..{:02x}{:02x}{:02x}",
            id[0], id[1], id[2], id[17], id[18], id[19],
        )
    }

    pub fn encode(&self) -> String {
        self.0
            .iter() .map(|b| format!("%{b:02X}"))
            .collect::<String>()
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.map(|b| format!("{b:02x}")).join(""))
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct EndpointV4(pub SocketAddrV4);
impl EndpointV4 {
    pub fn from_str(s: &str, port: u16) -> Option<Self> {
        s.parse::<Ipv4Addr>().ok().map(|ip| Self(SocketAddrV4::new(ip, port)))
    }
    
    pub fn from_compact(compact: &[u8; 6]) -> Self {
        Self(SocketAddrV4::new(
            Ipv4Addr::from_octets(compact[..4].try_into().unwrap()),
            u16::from_be_bytes(compact[4..6].try_into().unwrap()),
        ))
    }

    pub fn from_compact_list(compact: &[u8]) -> Result<Vec<Self>> {
        if !compact.len().is_multiple_of(6) {
            return Err(anyhow!("A compact list of IpV4 needs to be divisible by 6"));
        }
        Ok(compact.chunks_exact(6)
                .map(|c| EndpointV4::from_compact(c.try_into().unwrap()))
                .collect())
    }

    pub fn to_compact(&self) -> [u8; 6] {
        let mut compact = [0; 6];
        compact[..4].copy_from_slice(&self.0.ip().octets());
        compact[4..].copy_from_slice(&self.0.port().to_be_bytes());
        compact
    }
}
impl fmt::Display for EndpointV4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct EndpointV6(pub SocketAddrV6);
impl EndpointV6 {
    pub fn from_str(s: &str, port: u16) -> Option<Self> {
        s.parse::<Ipv6Addr>().ok().map(|ip| Self(SocketAddrV6::new(ip, port, 0, 0)))
    }

    pub fn from_compact(compact: &[u8; 6]) -> Self {
        let ip = Ipv6Addr::from_octets(compact[..16].try_into().unwrap());
        let port = u16::from_be_bytes(compact[16..18].try_into().unwrap());
        Self(SocketAddrV6::new(ip, port, 0, 0))
    }

    pub fn from_compact_list(compact: &[u8]) -> Result<Vec<Self>> {
        if !compact.len().is_multiple_of(18) {
            return Err(anyhow!("A compact list of IpV6 needs to be divisible by 18"));
        }
        Ok(compact.chunks_exact(18)
                .map(|c| EndpointV6::from_compact(c.try_into().unwrap()))
                .collect())
    }

    pub fn to_compact(&self) -> [u8; 18] {
        let mut compact = [0; 18];
        compact[..16].copy_from_slice(&self.0.ip().octets());
        compact[16..].copy_from_slice(&self.0.port().to_be_bytes());
        compact
    }
}
impl fmt::Display for EndpointV6 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Endpoint {
    V4(EndpointV4),
    V6(EndpointV6),
    Hostname {
        host: String,
        port: u16,
    },
}

impl Endpoint {
    pub fn from_str(s: &str, port: u16) -> Self {
        if let Some(e4) = EndpointV4::from_str(s, port) {
            Self::V4(e4)
        } else if let Some(e6) = EndpointV6::from_str(s, port) {
            Self::V6(e6)
        } else {
            Self::Hostname {
                host: s.to_string(),
                port,
            }
        }
    }

    pub fn from_compact_4(compact: &[u8]) -> Result<Self> {
        Ok(Self::V4(EndpointV4::from_compact(
            compact.try_into().map_err(|_| anyhow!("A compact IpV4 needs to be 6 bytes long"))?
        )))
    }

    pub fn from_compact_6(compact: &[u8]) -> Result<Self> {
        Ok(Self::V6(EndpointV6::from_compact(
            compact.try_into().map_err(|_| anyhow!("A compact IpV6 needs to be 6 bytes long"))?
        )))
    }

    pub fn from_compact_4_list(compact: &[u8]) -> Result<Vec<Self>> {
        Ok(EndpointV4::from_compact_list(compact)?.into_iter()
                .map(|v4| Self::V4(v4))
                .collect())
    }

    pub fn from_compact_6_list(compact: &[u8]) -> Result<Vec<Self>> {
        Ok(EndpointV6::from_compact_list(compact)?.into_iter()
                .map(|v6| Self::V6(v6))
                .collect())
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hostname { host, port } => write!(f, "{host}:{port}"),
            Self::V4(v4) => write!(f, "{v4}"),
            Self::V6(v6) => write!(f, "{v6}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub id: Option<PeerId>,
    pub endpoints: HashSet<Endpoint>,
    pub flags: u8,
}

impl fmt::Display for PeerInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (e, endpoint) in self.endpoints.iter().enumerate() {
            write!(f, "{}", endpoint)?;
            if e < self.endpoints.len() - 1 {
                write!(f, ", ")?;
            }
        }
        Ok(())
    }
}

impl PeerInfo {
    pub fn is_same_peer(&self, other: &Self) -> bool {
        match (self.id, other.id) {
            (Some(a), Some(b)) => a == b,
            _ => self.endpoints.intersection(&other.endpoints).next().is_some(),
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.id = self.id.or(other.id);
        self.flags |= other.flags;
        self.endpoints.extend(other.endpoints);
    }

    pub fn prefers_encryption(&self) -> bool {
        (self.flags & 0x01) >= 1
    }
    pub fn upload_only(&self) -> bool {
        (self.flags & 0x02) >= 1
    }
    pub fn supports_utp(&self) -> bool {
        (self.flags & 0x04) >= 1
    }
    pub fn holepunch_support(&self) -> bool {
        (self.flags & 0x08) >= 1
    }
    pub fn reachable(&self) -> bool {
        (self.flags & 0x10) >= 1
    }

    pub const DEFAULT_FLAGS: u8 = 0b00000000;

    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            id: None,
            flags: Self::DEFAULT_FLAGS,
            endpoints: HashSet::from_iter(vec![endpoint]),
        }
    }

    pub fn from_tracker_bencode_list(list: &BencodeAccessor) -> Result<Vec<Self>> {
        Ok(list.map_list(|peer| Ok(Self {
            id: Some(PeerId::from(peer.required(b"peer id")?.to_n_bytes()?)),
            flags: Self::DEFAULT_FLAGS,
            endpoints: HashSet::from_iter(vec![
                Endpoint::from_str(
                    peer.required(b"ip")?.as_str()?,
                    peer.required(b"port")?.to_integer()?,
                )
            ]),
        }))?)
    }

    pub fn from_tracker_bencode(root: &BencodeAccessor) -> Result<Vec<Self>> {
        use BencodeValue::*;
        let peers = root.required(b"peers")?;
        let mut infos = match peers.value {
            List(_) => Self::from_tracker_bencode_list(&peers),
            ByteString(peers) => {
                Ok(Endpoint::from_compact_4_list(peers)?.into_iter()
                    .map(Self::new).collect())
            }
            _ => Err(anyhow!("'peers' must be either a list or a byte string")),
        }?;
        if let Some(peers6) = root.optional(b"peers6")? {
            infos.extend(match peers6.value {
                List(_) => Self::from_tracker_bencode_list(&peers6),
                ByteString(peers) => {
                    Ok(Endpoint::from_compact_6(peers).into_iter()
                        .map(Self::new).collect())
                }
                _ => Err(anyhow!("'peers6' must be either a list or a byte string")),
            }?);
        }
        Ok(infos)
    }

    pub fn from_compact(compact: &[u8]) -> Vec<Self> {
        Endpoint::from_compact_4(compact).into_iter()
            .map(Self::new).collect()
    }

    pub async fn resolve(&self) -> Result<Vec<SocketAddr>> {
        let mut addrs = Vec::new();

        for endpoint in &self.endpoints {
            match endpoint {
                Endpoint::V4(v4) => addrs.push(SocketAddr::V4(v4.0)),
                Endpoint::V6(v6) => addrs.push(SocketAddr::V6(v6.0)),
                Endpoint::Hostname { host, port } => {
                    addrs.extend(
                        tokio::net::lookup_host((host.as_str(), *port)).await?
                    );
                }
            }
        }

        Ok(addrs)
    }
}

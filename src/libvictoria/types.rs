use anyhow::{Result, anyhow};
use rand::{Rng, RngExt};
use std::{
    collections::HashSet,
    fmt,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6, Ipv6Addr}
};

use bencodedit::{accessor::BencodeAccessor, BencodeValue};

pub type ClientVersion = Vec<u8>;
pub const CLIENT_VERSION: &'static [u8; 4] = &[b'L', b'V', 0, 0]; // libvictoria

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Kademlia<const N: usize>(pub [u8; N]);
impl<const N: usize> Kademlia<N> {
    pub fn prefix_length(&self) -> u8 {
        for (i, byte) in self.0.iter().enumerate() {
            if *byte != 0 {
                return i as u8 * 8 + byte.leading_zeros() as u8;
            }
        }
        (N * 8) as u8
    }
}

macro_rules! impl_id {
    ($ty:ty) => {
        impl AsRef<[u8; 20]> for $ty {
            fn as_ref(&self) -> &[u8; 20] { &self.0 }
        }
        impl From<$ty> for [u8; 20] {
            fn from(id: $ty) -> Self { id.0 }
        }
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0.map(|b| format!("{b:02x}")).join(""))
            }
        }
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{self}")
            }
        }
        impl $ty {
            pub fn kademlia(&self, other: impl AsRef<[u8; 20]>) -> Kademlia<20> {
                Kademlia(std::array::from_fn(|i| self.0[i] ^ other.as_ref()[i]))
            }
            pub fn to_vec(&self) -> Vec<u8> { self.0.to_vec() }
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(pub [u8; 20]);
impl Hash {
    pub fn from_xt(xt: &str) -> Result<Self> {
        let bytes: [u8; 20] = hex::decode(
            xt.strip_prefix("urn:btih:").ok_or(
                anyhow!("Only the btih format is supported for xt")
            )?
        )?.try_into().map_err(|_| anyhow!("Btih format requires 20 bytes"))?;
        Ok(Self(bytes))
    }

    pub fn encode(&self) -> String {
        self.0
            .iter() .map(|b| format!("%{b:02X}"))
            .collect::<String>()
    }
}
impl_id!(Hash);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub [u8; 20]);
impl PeerId {
    pub fn random() -> Self {
        let mut bytes = [0; 20];
        bytes[0..4].copy_from_slice(CLIENT_VERSION);
        rand::rng().fill_bytes(&mut bytes[4..]);
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
impl_id!(PeerId);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub [u8; 20]);
impl NodeId {
    pub fn random() -> Self {
        Self(rand::rng().random())
    }
}
impl_id!(NodeId);

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

impl From<std::net::SocketAddr> for Endpoint {
    fn from(addr: std::net::SocketAddr) -> Self {
        match addr {
            std::net::SocketAddr::V4(addr) => Self::V4(EndpointV4(addr)),
            std::net::SocketAddr::V6(addr) => Self::V6(EndpointV6(addr)),
        }
    }
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

    pub fn set_port(&mut self, new_port: u16) {
        match self {
            Self::Hostname { port, .. } => *port = new_port,
            Self::V4(v4) => v4.0.set_port(new_port),
            Self::V6(v6) => v6.0.set_port(new_port),
        }
    }

    pub async fn resolve(&self) -> Result<SocketAddr> {
        Ok(match self {
            Endpoint::V4(v4) => SocketAddr::V4(v4.0),
            Endpoint::V6(v6) => SocketAddr::V6(v6.0),
            Endpoint::Hostname { host, port } =>
                tokio::net::lookup_host((host.as_str(), *port)).await?
                    .next().ok_or(anyhow!("Failed to resolve hostname {host}"))?
        })
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
pub struct EndpointSet(HashSet<Endpoint>);
impl EndpointSet {
    pub fn intersects(&self, other: &Self) -> bool {
         !self.0.is_disjoint(&other.0)
    }

    pub fn merge(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    pub async fn resolve(&self) -> Result<Vec<SocketAddr>> {
        let mut addrs = Vec::new();
        for endpoint in self {
            addrs.push(endpoint.resolve().await?);
        }
        Ok(addrs)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Endpoint> {
        self.0.iter()
    }
}
impl From<Endpoint> for EndpointSet {
    fn from(endpoint: Endpoint) -> Self {
        Self(HashSet::from_iter([endpoint]))
    }
}
impl<'a> IntoIterator for &'a EndpointSet {
    type Item = &'a Endpoint;
    type IntoIter = std::collections::hash_set::Iter<'a, Endpoint>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}
impl fmt::Display for EndpointSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (e, endpoint) in self.0.iter().enumerate() {
            write!(f, "{}", endpoint)?;
            if e < self.0.len() - 1 {
                write!(f, ", ")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub id: Option<PeerId>,
    pub endpoints: EndpointSet,
    pub flags: u8,
}

impl fmt::Display for PeerInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}{}{}{}{})",
            self.endpoints,
            if self.prefers_encryption() {"E"} else {""},
            if self.upload_only() {"S"} else {""},
            if self.supports_utp() {"U"} else {""},
            if self.supports_holepunch() {"H"} else {""},
            if self.is_reachable() {"R"} else {""},
        )
    }
}

impl PeerInfo {
    pub fn is_same_peer(&self, other: &Self) -> bool {
        match (self.id, other.id) {
            (Some(a), Some(b)) => a == b,
            _ => self.endpoints.intersects(&other.endpoints),
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.id = self.id.or(other.id);
        self.flags |= other.flags;
        self.endpoints.merge(other.endpoints);
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
    pub fn supports_holepunch(&self) -> bool {
        (self.flags & 0x08) >= 1
    }
    pub fn is_reachable(&self) -> bool {
        (self.flags & 0x10) >= 1
    }

    pub const DEFAULT_FLAGS: u8 = 0b00000000;

    pub fn new(endpoint: Endpoint, flags: Option<u8>) -> Self {
        Self {
            id: None,
            flags: flags.unwrap_or(Self::DEFAULT_FLAGS),
            endpoints: EndpointSet::from(endpoint),
        }
    }

    pub fn from_tracker_bencode_list(list: &BencodeAccessor) -> Result<Vec<Self>> {
        Ok(list.map_list(|peer| Ok(Self {
            id: Some(PeerId(peer.required(b"peer id")?.to_n_bytes()?)),
            flags: Self::DEFAULT_FLAGS,
            endpoints: EndpointSet::from(
                Endpoint::from_str(
                    peer.required(b"ip")?.as_str()?,
                    peer.required(b"port")?.to_integer()?,
                )
            ),
        }))?)
    }

    pub fn from_tracker_bencode(root: &BencodeAccessor) -> Result<Vec<Self>> {
        use BencodeValue::*;
        let peers = root.required(b"peers")?;
        let mut infos = match peers.value {
            List(_) => Self::from_tracker_bencode_list(&peers),
            ByteString(peers) => {
                Ok(Endpoint::from_compact_4_list(peers)?.into_iter()
                    .map(Self::from).collect())
            }
            _ => Err(anyhow!("'peers' must be either a list or a byte string")),
        }?;
        if let Some(peers6) = root.optional(b"peers6")? {
            infos.extend(match peers6.value {
                List(_) => Self::from_tracker_bencode_list(&peers6),
                ByteString(peers) => {
                    Ok(Endpoint::from_compact_6(peers).into_iter()
                        .map(Self::from).collect())
                }
                _ => Err(anyhow!("'peers6' must be either a list or a byte string")),
            }?);
        }
        Ok(infos)
    }

    pub fn from_compact_4_list(compact: &[u8]) -> Result<Vec<Self>> {
        Ok(Endpoint::from_compact_4_list(compact)?.into_iter()
                .map(Self::from).collect())
    }
}

impl From<Endpoint> for PeerInfo {
    fn from(endpoint: Endpoint) -> Self {
        Self {
            endpoints: EndpointSet::from(endpoint),
            flags: Self::DEFAULT_FLAGS,
            id: None,
        }
    }
}
impl From<EndpointV4> for PeerInfo {
    fn from(endpoint: EndpointV4) -> Self {
        Self {
            endpoints: EndpointSet::from(Endpoint::V4(endpoint)),
            flags: Self::DEFAULT_FLAGS,
            id: None,
        }
    }
}
impl From<EndpointV6> for PeerInfo {
    fn from(endpoint: EndpointV6) -> Self {
        Self {
            endpoints: EndpointSet::from(Endpoint::V6(endpoint)),
            flags: Self::DEFAULT_FLAGS,
            id: None,
        }
    }
}

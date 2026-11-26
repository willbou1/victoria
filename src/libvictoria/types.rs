use std::{collections::HashSet, str::FromStr};

use anyhow::{Result, anyhow};
use rand::{Rng, RngExt, rng};
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};
use bencodedit::{BencodeValue, accessor::BencodeAccessor};
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
            fn as_ref(&self) -> &[u8; 20] {
                &self.0
            }
        }
        impl From<$ty> for [u8; 20] {
            fn from(id: $ty) -> Self {
                id.0
            }
        }
        impl std::str::FromStr for $ty {
            type Err = hex::FromHexError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let mut bytes = [0u8; 20];
                hex::decode_to_slice(s, &mut bytes)?;
                Ok(Self(bytes))
            }
        }
        impl TryFrom<&[u8]> for $ty {
            type Error = std::array::TryFromSliceError;

            fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
                Ok(Self(bytes.try_into()?))
            }
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
            pub fn kademlia(&self, other: &impl AsRef<[u8; 20]>) -> Kademlia<20> {
                Kademlia(std::array::from_fn(|i| self.0[i] ^ other.as_ref()[i]))
            }
            pub fn to_vec(&self) -> Vec<u8> {
                self.0.to_vec()
            }
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(pub [u8; 20]);
impl Hash {
    pub fn from_xt(xt: &str) -> Result<Self> {
        let bytes = xt
            .strip_prefix("urn:btih:")
            .ok_or(anyhow!("Only the btih format is supported for xt"))?
            .parse()?;
        Ok(bytes)
    }
}
impl_id!(Hash);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub [u8; 20]);
impl PeerId {
    pub fn random() -> Self {
        let mut bytes = [0; 20];
        bytes[0..4].copy_from_slice(CLIENT_VERSION);
        rng().fill_bytes(&mut bytes[4..]);
        Self(bytes)
    }
}
impl_id!(PeerId);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub [u8; 20]);
impl NodeId {
    pub fn random() -> Self {
        Self(rng().random())
    }

    pub fn random_with_prefix_length(&self, prefix_length: u8) -> Self {
        let mut bytes = self.0;
        let first = prefix_length as usize / 8;
        let bit = prefix_length % 8;
        if first < 20 {
            let mask = 0x80 >> bit;
            bytes[first] ^= mask;
            let random = rng().random::<u8>();
            let suffix_mask = mask - 1;
            bytes[first] = (bytes[first] & !suffix_mask) | (random & suffix_mask);
            rng().fill_bytes(&mut bytes[first + 1..]);
        }
        Self(bytes)
    }
}
impl_id!(NodeId);

pub trait SocketAddrV4Ext {
    fn from_compact(compact: &[u8; 6]) -> SocketAddrV4;
    fn from_compact_list(compact: &[u8]) -> Result<Vec<SocketAddrV4>>;
    fn to_compact(&self) -> [u8; 6];
}
impl SocketAddrV4Ext for SocketAddrV4 {
    fn from_compact(compact: &[u8; 6]) -> Self {
        Self::new(
            Ipv4Addr::from_octets(compact[..4].try_into().unwrap()),
            u16::from_be_bytes(compact[4..6].try_into().unwrap()),
        )
    }

    fn from_compact_list(compact: &[u8]) -> Result<Vec<Self>> {
        if !compact.len().is_multiple_of(6) {
            return Err(anyhow!("A compact list of IpV4 needs to be divisible by 6"));
        }
        Ok(compact
            .chunks_exact(6)
            .map(|c| Self::from_compact(c.try_into().unwrap()))
            .collect())
    }

    fn to_compact(&self) -> [u8; 6] {
        let mut compact = [0; 6];
        compact[..4].copy_from_slice(&self.ip().octets());
        compact[4..].copy_from_slice(&self.port().to_be_bytes());
        compact
    }
}

pub trait SocketAddrV6Ext {
    fn from_compact(compact: &[u8; 18]) -> SocketAddrV6;
    fn from_compact_list(compact: &[u8]) -> Result<Vec<SocketAddrV6>>;
    fn to_compact(&self) -> [u8; 18];
}
impl SocketAddrV6Ext for SocketAddrV6 {
    fn from_compact(compact: &[u8; 18]) -> Self {
        let ip = Ipv6Addr::from_octets(compact[..16].try_into().unwrap());
        let port = u16::from_be_bytes(compact[16..18].try_into().unwrap());
        Self::new(ip, port, 0, 0)
    }

    fn from_compact_list(compact: &[u8]) -> Result<Vec<Self>> {
        if !compact.len().is_multiple_of(18) {
            return Err(anyhow!(
                "A compact list of IpV6 needs to be divisible by 18"
            ));
        }
        Ok(compact
            .chunks_exact(18)
            .map(|c| Self::from_compact(c.try_into().unwrap()))
            .collect())
    }

    fn to_compact(&self) -> [u8; 18] {
        let mut compact = [0; 18];
        compact[..16].copy_from_slice(&self.ip().octets());
        compact[16..].copy_from_slice(&self.port().to_be_bytes());
        compact
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Endpoint {
    Addr(SocketAddr),
    Hostname { host: String, port: u16 },
}

impl From<std::net::SocketAddr> for Endpoint {
    fn from(addr: std::net::SocketAddr) -> Self {
        Self::Addr(addr)
    }
}
impl From<std::net::SocketAddrV4> for Endpoint {
    fn from(v4: std::net::SocketAddrV4) -> Self {
        Self::Addr(SocketAddr::V4(v4))
    }
}
impl From<std::net::SocketAddrV6> for Endpoint {
    fn from(v6: std::net::SocketAddrV6) -> Self {
        Self::Addr(SocketAddr::V6(v6))
    }
}
impl Endpoint {
    pub fn from_str(s: &str, port: u16) -> Self {
        if let Ok(ip) = IpAddr::from_str(s) {
            Self::Addr(SocketAddr::new(ip, port))
        } else {
            Self::Hostname {
                host: s.to_string(),
                port,
            }
        }
    }

    pub fn from_compact_4(compact: &[u8]) -> Result<Self> {
        Ok(SocketAddrV4::from_compact(
            compact
                .try_into()
                .map_err(|_| anyhow!("A compact IpV4 needs to be 6 bytes long"))?,
        ).into())
    }

    pub fn from_compact_6(compact: &[u8]) -> Result<Self> {
        Ok(SocketAddrV6::from_compact(
            compact
                .try_into()
                .map_err(|_| anyhow!("A compact IpV6 needs to be 6 bytes long"))?,
        ).into())
    }

    pub fn from_compact_4_list(compact: &[u8]) -> Result<Vec<Self>> {
        Ok(SocketAddrV4::from_compact_list(compact)?
            .into_iter()
            .map(|addr| addr.into())
            .collect())
    }

    pub fn from_compact_6_list(compact: &[u8]) -> Result<Vec<Self>> {
        Ok(SocketAddrV6::from_compact_list(compact)?
            .into_iter()
            .map(|addr| addr.into())
            .collect())
    }

    pub fn set_port(&mut self, new_port: u16) {
        match self {
            Self::Hostname { port, .. } => *port = new_port,
            Self::Addr(addr) => addr.set_port(new_port),
        }
    }

    pub async fn resolve(&self) -> Result<SocketAddr> {
        Ok(match self {
            Self::Addr(addr) => *addr,
            Self::Hostname { host, port } => tokio::net::lookup_host((host.as_str(), *port))
                .await?
                .next()
                .ok_or(anyhow!("Failed to resolve hostname {host}"))?,
        })
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hostname { host, port } => write!(f, "{host}:{port}"),
            Self::Addr(addr) => write!(f, "{addr}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointSet(pub HashSet<Endpoint>);
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
        write!(
            f,
            "{} ({}{}{}{}{})",
            self.endpoints,
            if self.prefers_encryption() { "E" } else { "" },
            if self.upload_only() { "S" } else { "" },
            if self.supports_utp() { "U" } else { "" },
            if self.supports_holepunch() { "H" } else { "" },
            if self.is_reachable() { "R" } else { "" },
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
        Ok(list.map_list(|peer| {
            Ok(Self {
                id: Some(PeerId(peer.required(b"peer id")?.to_n_bytes()?)),
                flags: Self::DEFAULT_FLAGS,
                endpoints: EndpointSet::from(Endpoint::from_str(
                    peer.required(b"ip")?.as_str()?,
                    peer.required(b"port")?.to_integer()?,
                )),
            })
        })?)
    }

    pub fn from_tracker_bencode(root: &BencodeAccessor) -> Result<Vec<Self>> {
        use BencodeValue::*;
        let peers = root.required(b"peers")?;
        let mut infos = match peers.value {
            List(_) => Self::from_tracker_bencode_list(&peers),
            ByteString(peers) => Ok(Endpoint::from_compact_4_list(peers)?
                .into_iter()
                .map(Self::from)
                .collect()),
            _ => Err(anyhow!("'peers' must be either a list or a byte string")),
        }?;
        if let Some(peers6) = root.optional(b"peers6")? {
            infos.extend(match peers6.value {
                List(_) => Self::from_tracker_bencode_list(&peers6),
                ByteString(peers) => Ok(Endpoint::from_compact_6(peers)
                    .into_iter()
                    .map(Self::from)
                    .collect()),
                _ => Err(anyhow!("'peers6' must be either a list or a byte string")),
            }?);
        }
        Ok(infos)
    }

    pub fn from_compact_4_list(compact: &[u8]) -> Result<Vec<Self>> {
        Ok(Endpoint::from_compact_4_list(compact)?
            .into_iter()
            .map(Self::from)
            .collect())
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
impl From<SocketAddr> for PeerInfo {
    fn from(addr: SocketAddr) -> Self {
        Self {
            endpoints: EndpointSet::from(Endpoint::from(addr)),
            flags: Self::DEFAULT_FLAGS,
            id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiscoveryMechanism {
    PEX,
    DHT,
    Tracker,
    Ingoing,
}

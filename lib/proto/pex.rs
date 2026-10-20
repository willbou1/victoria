use anyhow::{Result, Context};
use std::{
    collections::{HashSet, HashMap},
};

use crate::{
    bencode::{BencodeAccessor, BencodeValue},
    types::*,
};

// BEP 11: https://www.bittorrent.org/beps/bep_0011.html#bep-40

#[derive(Debug)]
pub struct PEXMessage {
    pub added: Vec<PeerInfo>,
    pub dropped: Vec<Endpoint>,
}

impl PEXMessage {
    pub fn from_bytes(encoded: &[u8]) -> Result<Self> {
        || -> Result<_> {
            let root_val = BencodeValue::from_bytes(encoded)?.0;
            let root = BencodeAccessor::new(&root_val);

            let mut added = Vec::new();
            let mut dropped = Vec::new();

            let flags4 = root.map_optional(b"added.f", |f| f.to_bytes())?;
            let flags6 = root.map_optional(b"added6.f", |f| f.to_bytes())?;

            if let Some(added_bytes) = &root.optional(b"added")? {
                added.extend(
                    Endpoint::from_compact_4(added_bytes.as_bytes()?)
                        .into_iter().enumerate().map(
                            |(e, endpoint)| PeerInfo {
                                endpoints: HashSet::from_iter(vec![endpoint]),
                                id: None,
                                flags: flags4.as_ref().map(|f| f[e])
                                    .unwrap_or(PeerInfo::DEFAULT_FLAGS),
                            })
                );
            }
            if let Some(added6_bytes) = &root.optional(b"added6")? {
                added.extend(
                    Endpoint::from_compact_6(added6_bytes.as_bytes()?)
                        .into_iter().enumerate().map(
                            |(e, endpoint)| PeerInfo {
                                endpoints: HashSet::from_iter(vec![endpoint]),
                                id: None,
                                flags: flags6.as_ref().map(|f| f[e])
                                    .unwrap_or(PeerInfo::DEFAULT_FLAGS),
                            })
                );
            }

            if let Some(dropped_bytes) = &root.optional(b"dropped")? {
                dropped.extend(Endpoint::from_compact_4(dropped_bytes.as_bytes()?));
            }
            if let Some(dropped6_bytes) = &root.optional(b"dropped6")? {
                dropped.extend(Endpoint::from_compact_4(dropped6_bytes.as_bytes()?));
            }

            Ok(Self {added, dropped})
        }().context("while parsing PEX message")
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        use BencodeValue::*;
        let mut added4 = Vec::new();
        let mut added6 = Vec::new();
        let mut flags4 = Vec::new();
        let mut flags6 = Vec::new();
        let mut dropped4 = Vec::new();
        let mut dropped6 = Vec::new();
        for added in &self.added {
            for endpoint in &added.endpoints {
                match endpoint {
                    Endpoint::Ip(std::net::SocketAddr::V4(_)) => {
                        added4.extend(endpoint.to_compact_4().unwrap());
                        flags4.push(added.flags);
                    }
                    Endpoint::Ip(std::net::SocketAddr::V6(_)) => {
                        added6.extend(endpoint.to_compact_6().unwrap());
                        flags6.push(added.flags);
                    }
                    _ => (),
                }
            }
        }
        for dropped in &self.dropped {
            match dropped {
                Endpoint::Ip(std::net::SocketAddr::V4(_)) => {
                    dropped4.extend(dropped.to_compact_4().unwrap());
                }
                Endpoint::Ip(std::net::SocketAddr::V6(_)) => {
                    dropped6.extend(dropped.to_compact_6().unwrap());
                }
                _ => (),
            }
        }
        Dictionary(HashMap::from([
            (Vec::from(b"added"), ByteString(added4)),
            (Vec::from(b"added6"), ByteString(added6)),
            (Vec::from(b"added.f"), ByteString(flags4)),
            (Vec::from(b"added6.f"), ByteString(flags6)),
            (Vec::from(b"dropped"), ByteString(dropped4)),
            (Vec::from(b"dropped6"), ByteString(dropped6)),
        ])).to_bytes()
    }
}

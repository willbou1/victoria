use anyhow::{Result, Context};
use std::{
    collections::{HashSet, HashMap},
};

use bencodedit::{accessor::BencodeAccessor, BencodeValue};

use crate::libvictoria::types::*;

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
                    Endpoint::from_compact_4_list(added_bytes.as_bytes()?)
                        .map_err(|e| added_bytes.wrap_err(e))?
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
                    Endpoint::from_compact_6_list(added6_bytes.as_bytes()?)
                        .map_err(|e| added6_bytes.wrap_err(e))?
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
                dropped.extend(
                    Endpoint::from_compact_4_list(dropped_bytes.as_bytes()?)
                        .map_err(|e| dropped_bytes.wrap_err(e))?
                );
            }
            if let Some(dropped6_bytes) = &root.optional(b"dropped6")? {
                dropped.extend(
                    Endpoint::from_compact_4_list(dropped6_bytes.as_bytes()?)
                        .map_err(|e| dropped6_bytes.wrap_err(e))?
                );
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
                    Endpoint::V4(v4) => {
                        added4.extend(v4.to_compact());
                        flags4.push(added.flags);
                    }
                    Endpoint::V6(v6) => {
                        added6.extend(v6.to_compact());
                        flags6.push(added.flags);
                    }
                    _ => (),
                }
            }
        }
        for dropped in &self.dropped {
            match dropped {
                Endpoint::V4(v4) => dropped4.extend(v4.to_compact()),
                Endpoint::V6(v6) => dropped6.extend(v6.to_compact()),
                _ => (),
            }
        }
        Dictionary(HashMap::from([
            (b"added".into(), ByteString(added4)),
            (b"added6".into(), ByteString(added6)),
            (b"added.f".into(), ByteString(flags4)),
            (b"added6.f".into(), ByteString(flags6)),
            (b"dropped".into(), ByteString(dropped4)),
            (b"dropped6".into(), ByteString(dropped6)),
        ])).to_bytes()
    }
}

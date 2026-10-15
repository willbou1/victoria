use anyhow::{Result, Context};
use std::{
    collections::{HashSet},
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
                                flags: flags4.as_ref().map(|f| f[e]),
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
                                flags: flags6.as_ref().map(|f| f[e]),
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
}

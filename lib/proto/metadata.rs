use anyhow::{Result, Context};
use std::{
    collections::HashMap,
};

use crate::{
    bencode::{BencodeValue, BencodeAccessor},
};

// BEP 9: https://www.bittorrent.org/beps/bep_0009.html 

#[derive(Debug)]
pub enum MetadataMessage {
    Request {
        index: usize,
    },
    Reject {
        index: usize,
    },
    Data {
        index: usize,
        total_size: usize,
        piece: Vec<u8>,
    },
    Unsupported {
        type_byte: u64,
    },
}

impl MetadataMessage {
    pub fn from_bytes(encoded: &[u8]) -> Result<Self> {
        || -> Result<_> {
            let decoded = BencodeValue::from_bytes(encoded)?;
            let root = BencodeAccessor::new(&decoded.0);

            Ok(match root.required(b"msg_type")?.to_integer()? {
                0 => Self::Request {
                    index: root.required(b"piece")?.to_integer()?,
                },
                1 => Self::Data {
                    index: root.required(b"piece")?.to_integer()?,
                    total_size: root.required(b"total_size")?.to_integer()?,
                    piece: decoded.1.to_vec(),
                },
                2 => Self::Reject {
                    index: root.required(b"piece")?.to_integer()?,
                },
                type_byte => Self::Unsupported { type_byte },
            })
        }().context("while parsing metadata message")
    }

    pub fn into_bytes(self) -> Vec<u8> {
        use BencodeValue::*;
        match self {
            Self::Request { index } => {
                Dictionary(HashMap::from([
                    (Vec::from(b"msg_type"), Integer(0)),
                    (Vec::from(b"piece"), Integer(index as i64)),
                ])).to_bytes()
            }
            Self::Data { index, total_size, piece } => {
                let mut ret = Dictionary(HashMap::from([
                    (Vec::from(b"msg_type"), Integer(1)),
                    (Vec::from(b"piece"), Integer(index as i64)),
                    (Vec::from(b"total_size"), Integer(total_size as i64)),
                ])).to_bytes();
                ret.extend(piece);
                ret
            }
            Self::Reject { index } => {
                Dictionary(HashMap::from([
                    (Vec::from(b"msg_type"), Integer(2)),
                    (Vec::from(b"piece"), Integer(index as i64)),
                ])).to_bytes()
            }
            _ => Vec::new(),
        }
    }
}

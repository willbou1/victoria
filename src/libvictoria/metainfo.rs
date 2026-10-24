use anyhow::{Result, anyhow, Context};
use std::{
    path::PathBuf,
    collections::HashMap,
    range::Range,
};

use bencodedit::{BencodeValue, accessor::BencodeAccessor};

use crate::libvictoria::{
    types::*,
};

const LABEL_WIDTH: usize = 20;
const INDENT: &str = "    ";

pub struct MetainfoFile {
    pub path: PathBuf,
    pub length: usize,

    // derived from metadata
    pub pieces: Range<usize>,
}

impl MetainfoFile {
    fn from_bencode(root: &BencodeAccessor, name: &str) -> Result<Vec<Self>> {
        match (root.optional(b"length")?, root.optional(b"files")?) {
            (Some(_), Some(_)) => return Err(anyhow!("only one of 'length' or 'files' must be set")),
            (Some(length), _) => Ok(vec![
                MetainfoFile {
                    path: PathBuf::from(name),
                    length: length.to_integer()?,
                    pieces: Default::default(),
                }
            ]),
            (_, Some(files)) => Ok(
                files.map_list(|file| Ok(Self {
                    path: PathBuf::from(name).join(
                        file.required(b"path")?.as_str_list()?.join("/")
                    ),
                    length: file.required(b"length")?.to_integer()?,
                    pieces: Default::default(),
                }))?
            ),
            _ => root.err_out(anyhow!("either 'length' or 'files' must be set"))?,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PieceFile {
    pub file_index: usize,
    pub piece_offset: usize,
    pub file_offset: usize,
    pub length: usize,
}

pub struct Metadata {
    pub name: String,
    pub piece_length: usize,
    pub pieces: Vec<Hash>,
    pub files: Vec<MetainfoFile>,
    pub private: bool,
    value: BencodeValue,

    // derived from metadata
    pub num_pieces: usize,
    pub length: usize,
    pub last_piece_length: usize,
    pub piece_files: Vec<Vec<PieceFile>>,
}

impl Metadata {
    pub fn from_bytes(encoded: &[u8]) -> Result<Self> {
        || -> Result<_> {
            let info_val = BencodeValue::from_bytes(encoded)?.0;
            let info = BencodeAccessor::new(&info_val);
            let name = info.required(b"name")?.to_string()?;
            let mut files = MetainfoFile::from_bencode(&info, &name)?;
            let piece_length: usize = info.required(b"piece length")?.to_integer()?;
            let pieces: Vec<_> = info.required(b"pieces")?.as_bytes()?
                .chunks(20).map(|chunk| Hash::from(chunk.try_into().unwrap()))
                .collect();

            let num_pieces = pieces.len();
            let length = files.iter().fold(0, |a, e| e.length + a);
            let last_piece_length = length - piece_length * (num_pieces - 1);

            let mut piece_files = Vec::new();
            let mut first_file_pieces = HashMap::new();
            for p in 0..num_pieces {
                let mut file_offset = 0;
                let piece_start = p * piece_length;
                let piece_length = if p == num_pieces - 1 {last_piece_length} else {piece_length};
                let piece_end = piece_start + piece_length;
                let mut files_for_piece = Vec::new();
                for (f, file) in files.iter().enumerate() {
                    let file_start = file_offset;
                    let file_end = file_start + file.length;
                    let overlap_start = piece_start.max(file_start);
                    let overlap_end = piece_end.min(file_end);
                    if overlap_start < overlap_end {
                        if !first_file_pieces.contains_key(&f) {
                            first_file_pieces.insert(f, p);
                        }

                        files_for_piece.push(PieceFile {
                            file_index: f,
                            piece_offset: overlap_start - piece_start,
                            file_offset: overlap_start - file_start,
                            length: overlap_end - overlap_start,
                        });
                    }
                    file_offset = file_end;
                }

                for f in first_file_pieces.keys().copied()
                    .filter(|&f| files_for_piece.iter().all(|pf| pf.file_index != f) || p == num_pieces - 1)
                    .collect::<Vec<_>>()
                {
                    if let Some(first_piece) = first_file_pieces.remove(&f) {
                        files[f].pieces = Range::from(first_piece..p + 1);
                    }
                }
                piece_files.push(files_for_piece);
            }

            Ok(Self {
                name,
                piece_length,
                pieces,
                files,
                private: info.map_optional(b"private", |p| p.to_bool())?
                    .unwrap_or(false),
                value: info_val,

                length,
                last_piece_length,
                num_pieces,
                piece_files,
            })
        }().context("while parsing metadata")
    }

   fn to_bencode(&self) -> BencodeValue {
       // We just use the original bencode value to ensure the hash is preserved
       self.value.clone()
    } 

    pub fn piece_length(&self, index: usize) -> usize {
        if index == self.num_pieces - 1 {
            self.last_piece_length
        } else {
            self.piece_length
        }
    }
}

#[derive(Clone)]
pub struct Metainfo {
    pub announces: Vec<Vec<String>>,

    pub created_by: Option<String>,
    pub comment: Option<String>,
}

impl Metainfo {
    pub fn from_magnet(trackers: Vec<String>) -> Self {
        Self {
            announces: vec![trackers],
            created_by: None,
            comment: None,
        }
    }

    pub fn from_bytes(encoded: &[u8]) -> Result<(Self, Option<Vec<u8>>)> {
        || -> Result<_> {
            let root_val = BencodeValue::from_bytes(encoded)?.0;
            let root = BencodeAccessor::new(&root_val);
            let info_bytes = root.optional(b"info")?.map(|i| i.value.to_bytes());

            Ok((Metainfo {
                announces: match root.optional(b"announce-list")? {
                    Some(announce_list) => announce_list
                        .map_list(|tier| tier.to_string_list())?,
                    None => vec![vec![root.required(b"announce")?.to_string()?]],
                },
                created_by: root.map_optional(b"created by", |c| c.to_string())?,
                comment: root.map_optional(b"comment", |c| c.to_string())?,
            }, info_bytes))
        }().context("while parsing metainfo")
    }

    pub fn to_bytes(&self, metadata: Option<&Metadata>) -> Vec<u8> {
        use BencodeValue::*;
        let mut root = HashMap::new();

        if self.announces.len() == 1 && self.announces[0].len() == 1 {
            root.insert(b"announce".into(),
                ByteString(self.announces[0][0].as_bytes().to_vec()),
            );
        } else {
            root.insert(b"announce-list".into(),
                List(
                    self.announces.iter()
                        .map(|tier| List(
                            tier.iter()
                                .map(|url| ByteString(url.as_bytes().to_vec()))
                                .collect(),
                        ))
                        .collect(),
                ),
            );
        }
        if let Some(created_by) = &self.created_by {
            root.insert(b"created by".into(),
                ByteString(created_by.as_bytes().to_vec()),
            );
        }
        if let Some(comment) = &self.comment {
            root.insert(b"comment".into(),
                ByteString(comment.as_bytes().to_vec()),
            );
        }
        if let Some(metadata) = metadata {
            root.insert(b"info".into(),
                metadata.to_bencode(),
            );
        }
        Dictionary(root).to_bytes()
    }
}

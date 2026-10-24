use anyhow::{Result, anyhow};
use tokio::{
    net::UdpSocket,
};
use std::collections::{HashMap};

use bencodedit::{accessor::BencodeAccessor, BencodeValue};

use crate::libvictoria::types::*;

type NodeId = Id;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    pub id: NodeId,
    pub endpoint: EndpointV4,
}

impl NodeInfo {
    pub fn from_compact(compact: &[u8]) -> Result<Self> {
        if compact.len() != 26 {
            return Err(anyhow!("A compact node needs to be 26 bytes long"));
        }
        Ok(Self {
            id: NodeId::from(compact[0..20].try_into().unwrap()),
            endpoint: EndpointV4::from_compact(&compact[20..26].try_into().unwrap()),
        })
    }
    
    pub fn from_compact_list(compact: &[u8]) -> Result<Vec<Self>> {
        if !compact.len().is_multiple_of(26) {
            return Err(anyhow!("A list of compact nodes need to be divisible by 26"));
        }
        Ok(compact.chunks_exact(26)
            .map(|c| Self::from_compact(c).unwrap())
            .collect())
    }

    pub fn to_compact(&self) -> Vec<u8> {
        let mut compact = Vec::new();
        compact.extend(self.id.as_bytes());
        compact.extend(self.endpoint.to_compact());
        compact
    }
}

struct Message {
    transaction_id: Vec<u8>,
    client_version: ClientVersion,
    type_of: MessageType,
}

impl Message {
    pub fn from_bytes(encoded: &[u8]) -> Result<Self> {
        let root_val = BencodeValue::from_bytes(encoded)?.0;
        let root = BencodeAccessor::new(&root_val);
        Ok(Self {
            transaction_id: root.required(b"t")?.to_bytes()?,
            client_version: root.required(b"v")?.to_n_bytes::<2>()?,
            type_of: MessageType::from_bencode(&root)?,
        })
    }

    pub fn to_bytes(self) -> Vec<u8> {
        use BencodeValue::*;
        let mut key_vals = HashMap::from([
            (b"t".into(), ByteString(self.transaction_id)),
            (b"v".into(), ByteString(self.client_version.to_vec())),
        ]);
        self.type_of.fill_bencode(&mut key_vals);
        Dictionary(key_vals).to_bytes()
    }
}

enum ErrorCode {
    Generic = 201,
    Server = 202,
    Protocol = 203,
    UnknownMethod = 204,
}

impl TryFrom<u8> for ErrorCode {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            201 => Ok(Self::Generic),
            202 => Ok(Self::Server),
            203 => Ok(Self::Protocol),
            204 => Ok(Self::UnknownMethod),
            _ => Err(anyhow!("invalid error code {value}")),
        }
    }
}

enum PeersOrNodes {
    Peers(Vec<EndpointV4>),
    Nodes(Vec<NodeInfo>),
}

enum MessageType {
    Query {
        id: NodeId,
        query: Query,
    },
    Response {
        id: NodeId,
        response: Response,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

impl MessageType {
    pub fn from_bencode(root: &BencodeAccessor) -> Result<Self> {
        let y = root.required(b"y")?;
        Ok(match y.as_n_bytes::<1>()? {
            b"q" => Self::Query {
                id: NodeId::from(root.required(b"a")?.required(b"id")?.to_n_bytes()?),
                query: Query::from_bencode(root)?,
            },
            b"r" => Self::Response {
                id: NodeId::from(root.required(b"a")?.required(b"id")?.to_n_bytes()?),
                response: Response::from_bencode(root)?,
            },
            b"e" => {
                let error = root.required(b"e")?;
                Self::Error {
                    code: {
                        let code = error.at(0)?;
                        code.to_integer::<u8>()?.try_into()
                            .map_err(|e| code.wrap_err(e))?
                    },
                    message: error.at(1)?.to_string()?,
                }
            },
            t => y.err_out(anyhow!("invalid message type {t:?}"))?,
        })
    }

    pub fn fill_bencode(self, key_vals: &mut HashMap<Vec<u8>, BencodeValue>) {
        use BencodeValue::*;
        match self {
            Self::Error { code, message } => {
                key_vals.insert(b"y".into(), ByteString(b"e".to_vec()));
                key_vals.insert(b"e".into(), List(vec![
                    Integer(code as i64),
                    ByteString(message.bytes().collect())
                ]));
            }
            Self::Query { id, query } => {
                key_vals.insert(b"y".into(), ByteString(b"q".to_vec()));
                query.fill_bencode(key_vals, id);
            }
            Self::Response { id, response } => {
                key_vals.insert(b"y".into(), ByteString(b"r".to_vec()));
                response.fill_bencode(key_vals, id);
            }
        }
    }
}

enum Query {
    Ping,
    AnnouncePeer {
        implied_port: bool,
        info_hash: Hash,
        port: u16,
        token: Vec<u8>,
    },
    GetPeers {
        info_hash: Hash,
    },
    FindNode {
        target: NodeId,
    },
}

impl Query {
    pub fn from_bencode(root: &BencodeAccessor) -> Result<Self> {
        let params = root.required(b"a")?;
        let q = root.required(b"q")?;
        Ok(match q.required(b"q")?.as_str()? {
            "ping" => Self::Ping,
            "find_node" => Self::FindNode {
                target: NodeId::from(params.required(b"target")?.to_n_bytes()?),
            },
            "get_peers" => Self::GetPeers {
                info_hash: Hash::from(params.required(b"id")?.to_n_bytes()?),
            },
            "announce_peer" => Self::AnnouncePeer {
                info_hash: Hash::from(params.required(b"info_hash")?.to_n_bytes()?),
                implied_port: params.required(b"implied_port")?.to_bool()?,
                port: params.required(b"port")?.to_integer()?,
                token: params.required(b"token")?.to_bytes()?,
            },
            t => q.err_out(anyhow!("invalid query type {t}"))?,
        })
    }

    pub fn fill_bencode(self, key_vals: &mut HashMap<Vec<u8>, BencodeValue>, id: NodeId) {
        use BencodeValue::*;
        let mut params = HashMap::from([
            (b"id".into(), ByteString(id.as_bytes().to_vec()))
        ]);
        match self {
            Self::Ping => {
                key_vals.insert(b"q".into(), ByteString(b"ping".to_vec()));
            },
            Self::FindNode { target } => {
                key_vals.insert(b"q".into(), ByteString(b"find_node".to_vec()));
                params.insert(b"target".into(), ByteString(target.as_bytes().to_vec()));
            }
            Self::GetPeers { info_hash } => {
                key_vals.insert(b"q".into(), ByteString(b"get_peers".to_vec()));
                params.insert(b"info_hash".into(), ByteString(info_hash.as_bytes().to_vec()));
            }
            Self::AnnouncePeer { implied_port, info_hash, port, token } => {
                key_vals.insert(b"q".into(), ByteString(b"announce_peer".to_vec()));
                params.insert(b"info_hash".into(), ByteString(info_hash.as_bytes().to_vec()));
                params.insert(b"token".into(), ByteString(token));
                params.insert(b"implied_port".into(), Integer(if implied_port {1} else {0}));
                params.insert(b"port".into(), Integer(port as i64));
            }
        }
        key_vals.insert(b"a".into(), Dictionary(params));
    }
}

enum Response {
    Ping,
    AnnouncePeer,
    GetPeers {
        token: Vec<u8>,
        peers_or_nodes: PeersOrNodes,
    },
    FindNode {
        nodes: Vec<NodeInfo>,
    },
}

impl Response {
    pub fn from_bencode(root: &BencodeAccessor) -> Result<Self> {
        use PeersOrNodes::*;
        let params = root.required(b"a")?;
        let r = root.required(b"r")?;
        Ok(match r.as_str()? {
            "ping" => Self::Ping,
            "announce_peer" => Self::AnnouncePeer,
            "find_node" => {
                let nodes = params.required(b"nodes")?;
                Self::FindNode {
                    nodes: NodeInfo::from_compact_list(nodes.as_bytes()?)
                        .map_err(|e| nodes.wrap_err(e))?,
                }
            },
            "get_peers" => Self::GetPeers {
                token: params.required(b"token")?.to_bytes()?,
                peers_or_nodes: match (params.optional(b"values")?, params.optional(b"nodes")?) {
                    (Some(values), None) => Peers(
                        EndpointV4::from_compact_list(values.as_bytes()?)
                            .map_err(|e| values.wrap_err(e))?
                    ),
                    (None, Some(nodes)) => Nodes(
                        NodeInfo::from_compact_list(nodes.as_bytes()?)
                            .map_err(|e| nodes.wrap_err(e))?
                    ),
                    _ => params.err_out(anyhow!("One key named values or nodes is required"))?,
                }
            },
            t => r.err_out(anyhow!("invalid response type {t}"))?,
        })
    }

    pub fn fill_bencode(self, key_vals: &mut HashMap<Vec<u8>, BencodeValue>, id: NodeId) {
        use BencodeValue::*;
        let mut params = HashMap::from([
            (b"id".into(), ByteString(id.into_bytes().to_vec()))
        ]);
        match self {
            Self::Ping => {
                key_vals.insert(b"r".into(), ByteString(b"ping".to_vec()));
            },
            Self::FindNode { nodes } => {
                key_vals.insert(b"r".into(), ByteString(b"find_node".to_vec()));
                params.insert(b"nodes".into(), ByteString(
                    nodes.iter().flat_map(|n| n.to_compact()).collect()
                ));
            },
            Self::AnnouncePeer => {
                key_vals.insert(b"r".into(), ByteString(b"announce_peer".to_vec()));
            }
            Self::GetPeers { token, peers_or_nodes } => {
                key_vals.insert(b"r".into(), ByteString(b"get_peers".to_vec()));
                params.insert(b"token".into(), ByteString(token));
                match peers_or_nodes {
                    PeersOrNodes::Nodes(nodes) => {
                        params.insert(b"nodes".to_vec(),  ByteString(
                            nodes.iter().flat_map(|n| n.to_compact()).collect()
                        ));
                    }
                    PeersOrNodes::Peers(peers) => {
                        params.insert(b"peers".to_vec(),  ByteString(
                            peers.iter().flat_map(|n| n.to_compact()).collect()
                        ));
                    }
                }
            }
        }
        key_vals.insert(b"a".to_vec(), Dictionary(params));
    }
}

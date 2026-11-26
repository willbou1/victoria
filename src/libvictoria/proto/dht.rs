use anyhow::{Context, Result, anyhow};
use tracing::trace;

use rand::{RngExt, rng};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};
use tokio::net::UdpSocket;

use bencodedit::{BencodeValue, accessor::BencodeAccessor};

use crate::libvictoria::types::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    pub id: NodeId,
    pub addrs: HashSet<SocketAddr>,
}

impl NodeInfo {
    pub fn new(id: NodeId, addr: SocketAddr) -> Self {
        Self {
            id,
            addrs: HashSet::from([addr]),
        }
    }

    pub fn is_same(&self, other: &Self) -> bool {
        self.id == other.id || !self.addrs.is_disjoint(&other.addrs)
    }

    pub fn merge(&mut self, other: Self) {
        self.addrs.extend(other.addrs);
    }

    pub fn from_compact_4(compact: &[u8]) -> Result<Self> {
        if compact.len() != 26 {
            return Err(anyhow!("a compact4 node needs to be 26 bytes long"));
        }
        Ok(Self {
            id: NodeId(compact[0..20].try_into().unwrap()),
            addrs: HashSet::from([SocketAddr::V4(SocketAddrV4::from_compact(
                &compact[20..26].try_into()?,
            ))]),
        })
    }

    pub fn from_compact_6(compact: &[u8]) -> Result<Self> {
        if compact.len() != 38 {
            return Err(anyhow!("a compact6 node needs to be 26 bytes long"));
        }
        Ok(Self {
            id: NodeId(compact[0..20].try_into().unwrap()),
            addrs: HashSet::from([SocketAddr::V6(SocketAddrV6::from_compact(
                &compact[20..38].try_into()?,
            ))]),
        })
    }

    pub fn from_compact_lists(
        compact4s: Option<&[u8]>,
        compact6s: Option<&[u8]>,
    ) -> Result<Vec<Self>> {
        let mut infos = Vec::<Self>::new();
        if let Some(compact4s) = compact4s {
            if !compact4s.len().is_multiple_of(26) {
                return Err(anyhow!(
                    "a list of compact4 nodes need to be divisible by 26, got {}",
                    compact4s.len(),
                ));
            }
            for info in compact4s
                .chunks_exact(26)
                .map(|c| Self::from_compact_4(c).unwrap())
            {
                if let Some(present) = infos.iter_mut().find(|i| i.is_same(&info)) {
                    present.merge(info);
                } else {
                    infos.push(info);
                }
            }
        }
        if let Some(compact6s) = compact6s {
            if !compact6s.len().is_multiple_of(38) {
                return Err(anyhow!(
                    "a list of compact6 nodes need to be divisible by 38, got {}",
                    compact6s.len(),
                ));
            }
            for info in compact6s
                .chunks_exact(38)
                .map(|c| Self::from_compact_6(c).unwrap())
            {
                if let Some(present) = infos.iter_mut().find(|i| i.is_same(&info)) {
                    present.merge(info);
                } else {
                    infos.push(info);
                }
            }
        }
        Ok(infos)
    }

    pub fn v4(&self) -> Option<SocketAddrV4> {
        for addr in &self.addrs {
            match addr {
                SocketAddr::V4(v4) => {
                    return Some(*v4);
                }
                _ => (),
            }
        }
        None
    }

    pub fn v6(&self) -> Option<SocketAddrV6> {
        for addr in &self.addrs {
            match addr {
                SocketAddr::V6(v6) => {
                    return Some(*v6);
                }
                _ => (),
            }
        }
        None
    }

    pub fn to_compact_4(&self) -> Option<Vec<u8>> {
        if let Some(v4) = self.v4() {
            let mut compact = Vec::new();
            compact.extend(self.id.as_ref());
            compact.extend(v4.to_compact());
            return Some(compact);
        }
        None
    }

    pub fn to_compact_6(&self) -> Option<Vec<u8>> {
        if let Some(v6) = self.v6() {
            let mut compact = Vec::new();
            compact.extend(self.id.as_ref());
            compact.extend(v6.to_compact());
            return Some(compact);
        }
        None
    }
}

type TransactionId = Vec<u8>;
pub struct Message {
    pub transaction_id: TransactionId,
    pub client_version: Option<ClientVersion>,
    pub type_of: MessageType,
}

impl Message {
    pub fn from_bytes<F>(encoded: &[u8], mut f: F) -> Result<Self>
    where
        F: FnMut(&BencodeAccessor, char, &TransactionId) -> Result<MessageType>,
    {
        let root_val = BencodeValue::from_bytes(encoded)?.0;
        let root = BencodeAccessor::new(&root_val);
        let transaction_id = root.required(b"t")?.to_bytes()?;
        let message_type = root.required(b"y")?.to_char()?;
        Ok(Self {
            client_version: root.map_optional(b"v", |v| v.to_bytes())?,
            type_of: f(&root, message_type, &transaction_id)?,
            transaction_id,
        })
    }

    pub fn to_bytes(self, v6: bool) -> Vec<u8> {
        use BencodeValue::*;
        let mut key_vals = HashMap::from([(b"t".into(), ByteString(self.transaction_id))]);
        if let Some(client_version) = self.client_version {
            key_vals.insert(b"v".into(), ByteString(client_version.to_vec()));
        }
        self.type_of.fill_bencode(&mut key_vals, v6);
        Dictionary(key_vals).to_bytes()
    }
}

#[derive(Debug)]
pub enum ErrorCode {
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

pub enum MessageType {
    Query { id: NodeId, query: Query },
    Response { id: NodeId, response: Response },
    Error { code: ErrorCode, message: String },
}

impl MessageType {
    fn from_error_bencode(error: BencodeAccessor) -> Result<Self> {
        Ok(Self::Error {
            code: {
                let code = error.at(0)?;
                code.to_integer::<u8>()?
                    .try_into()
                    .map_err(|e| code.wrap_err(e))?
            },
            message: error.at(1)?.to_string()?,
        })
    }

    pub fn from_bencode(root: &BencodeAccessor, v6: bool) -> Result<Self> {
        let y = root.required(b"y")?;
        Ok(match y.to_char()? {
            'q' => Self::Query {
                id: NodeId(root.required(b"a")?.required(b"id")?.to_n_bytes()?),
                query: Query::from_bencode(root)?,
            },
            'r' => match (root.optional(b"r")?, root.optional(b"e")?) {
                (Some(response), None) => Self::Response {
                    id: NodeId(response.required(b"id")?.to_n_bytes()?),
                    response: Response::from_bencode(&response, v6)?,
                },
                (None, Some(error)) => Self::from_error_bencode(error)?,
                _ => root.err_out(anyhow!(
                    "a response needs either a response or an error body"
                ))?,
            },
            'e' => Self::from_error_bencode(root.required(b"e")?)?,
            t => y.err_out(anyhow!("invalid message type {t} expected r, q or e"))?,
        })
    }

    pub fn fill_bencode(self, key_vals: &mut HashMap<Vec<u8>, BencodeValue>, v6: bool) {
        use BencodeValue::*;
        match self {
            Self::Error { code, message } => {
                key_vals.insert(b"y".into(), ByteString(b"e".to_vec()));
                key_vals.insert(
                    b"e".into(),
                    List(vec![
                        Integer(code as i64),
                        ByteString(message.bytes().collect()),
                    ]),
                );
            }
            Self::Query { id, query } => {
                key_vals.insert(b"y".into(), ByteString(b"q".to_vec()));
                query.fill_bencode(key_vals, id);
            }
            Self::Response { id, response } => {
                key_vals.insert(b"y".into(), ByteString(b"r".to_vec()));
                key_vals.insert(b"r".into(), response.to_bencode(id, v6));
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct WantNodes {
    pub nodes4: bool,
    pub nodes6: bool,
}

impl Default for WantNodes {
    fn default() -> Self {
        Self {
            nodes4: false,
            nodes6: false,
        }
    }
}

impl WantNodes {
    pub fn new(nodes4: bool, nodes6: bool) -> Self {
        Self { nodes4, nodes6 }
    }

    pub fn from_bencode(want: Option<BencodeAccessor>) -> Result<Self> {
        let mut want_nodes = Self::default();
        Ok(match want {
            Some(want) => {
                let list = want.as_str_list()?;
                match (list.contains(&"n4"), list.contains(&"n6")) {
                    (false, false) => want.err_out(anyhow!("must contain at least n4 or n6"))?,
                    (n4, n6) => {
                        want_nodes.nodes4 = n4;
                        want_nodes.nodes6 = n6;
                    }
                }
                want_nodes
            }
            None => want_nodes,
        })
    }

    pub fn to_bencode(&self) -> Option<BencodeValue> {
        use BencodeValue::*;
        if !self.nodes4 && !self.nodes6 {
            return None;
        }
        let mut want = Vec::new();
        if self.nodes4 {
            want.push(ByteString(b"n4".to_vec()));
        }
        if self.nodes6 {
            want.push(ByteString(b"n6".to_vec()));
        }
        Some(List(want))
    }
}

#[derive(Debug, Clone)]
pub enum Query {
    Ping,
    AnnouncePeer {
        implied_port: bool,
        info_hash: Hash,
        port: u16,
        token: Vec<u8>,
    },
    GetPeers {
        info_hash: Hash,
        want_nodes: WantNodes,
    },
    FindNode {
        target: NodeId,
        want_nodes: WantNodes,
    },
    SampleInfohashes {
        target: Hash,
    },
}

impl Query {
    pub fn from_bencode(root: &BencodeAccessor) -> Result<Self> {
        let q = root.required(b"q")?;
        let params = root.required(b"a")?;
        Ok(match q.as_str()? {
            "ping" => Self::Ping,
            "sample_infohashes" => Self::SampleInfohashes {
                target: Hash(params.required(b"target")?.to_n_bytes()?),
            },
            "find_node" => Self::FindNode {
                target: NodeId(params.required(b"target")?.to_n_bytes()?),
                want_nodes: WantNodes::from_bencode(params.optional(b"want")?)?,
            },
            "get_peers" => Self::GetPeers {
                info_hash: Hash(params.required(b"info_hash")?.to_n_bytes()?),
                want_nodes: WantNodes::from_bencode(params.optional(b"want")?)?,
            },
            "announce_peer" => Self::AnnouncePeer {
                info_hash: Hash(params.required(b"info_hash")?.to_n_bytes()?),
                implied_port: params.map_optional(b"implied_port", |ip| ip.to_bool())?.unwrap_or(false),
                port: params.required(b"port")?.to_integer()?,
                token: params.required(b"token")?.to_bytes()?,
            },
            t => q.err_out(anyhow!("invalid query type {t}"))?,
        })
    }

    pub fn fill_bencode(self, key_vals: &mut HashMap<Vec<u8>, BencodeValue>, id: NodeId) {
        use BencodeValue::*;
        let mut params = HashMap::from([(b"id".into(), ByteString(id.to_vec()))]);
        match self {
            Self::Ping => {
                key_vals.insert(b"q".into(), ByteString(b"ping".to_vec()));
            }
            Self::SampleInfohashes { target } => {
                key_vals.insert(b"q".into(), ByteString(b"sample_infohashes".to_vec()));
                params.insert(b"target".into(), ByteString(target.to_vec()));
            }
            Self::FindNode { target, want_nodes } => {
                key_vals.insert(b"q".into(), ByteString(b"find_node".to_vec()));
                params.insert(b"target".into(), ByteString(target.to_vec()));
                if let Some(want) = want_nodes.to_bencode() {
                    params.insert(b"want".into(), want);
                }
            }
            Self::GetPeers {
                info_hash,
                want_nodes,
            } => {
                key_vals.insert(b"q".into(), ByteString(b"get_peers".to_vec()));
                params.insert(b"info_hash".into(), ByteString(info_hash.to_vec()));
                if let Some(want) = want_nodes.to_bencode() {
                    params.insert(b"want".into(), want);
                }
            }
            Self::AnnouncePeer {
                implied_port,
                info_hash,
                port,
                token,
            } => {
                key_vals.insert(b"q".into(), ByteString(b"announce_peer".to_vec()));
                params.insert(b"info_hash".into(), ByteString(info_hash.to_vec()));
                params.insert(b"token".into(), ByteString(token));
                params.insert(
                    b"implied_port".into(),
                    Integer(if implied_port { 1 } else { 0 }),
                );
                params.insert(b"port".into(), Integer(port as i64));
            }
        }
        key_vals.insert(b"a".into(), Dictionary(params));
    }
}

#[derive(Debug)]
pub struct Response {
    pub token: Option<Vec<u8>>,
    pub nodes: Option<Vec<NodeInfo>>,
    pub values: Option<HashSet<SocketAddr>>,

    pub interval: Option<usize>,
    pub num: Option<usize>,
    pub samples: Option<HashSet<Hash>>,
}

impl Response {
    pub const PING: Self = Self {
        interval: None,
        nodes: None,
        num: None,
        samples: None,
        token: None,
        values: None,
    };

    pub fn find_nodes(nodes: Vec<NodeInfo>) -> Self {
        Self {
            nodes: if nodes.len() > 0 {Some(nodes)} else {None},
            ..Self::PING
        }
    }

    pub fn get_peers_nodes(token: Vec<u8>, nodes: Vec<NodeInfo>) -> Self {
        Self {
            token: Some(token),
            nodes: if nodes.len() > 0 {Some(nodes)} else {None},
            ..Self::PING
        }
    }

    pub fn get_peers_values(token: Vec<u8>, values: HashSet<SocketAddr>) -> Self {
        Self {
            token: Some(token),
            values: Some(values),
            ..Self::PING
        }
    }

    pub fn from_bencode(response: &BencodeAccessor, v6: bool) -> Result<Self> {
        Ok(Self {
            token: response.map_optional(b"token", |token| token.to_bytes())?,
            values: response
                .map_optional(b"values", |values| {
                    values.map_list(|value| {
                        Ok(if v6 {
                            SocketAddrV6::from_compact(value.as_n_bytes()?).into()
                        } else {
                            SocketAddrV4::from_compact(value.as_n_bytes()?).into()
                        })
                    })
                })?
                .map(|v| HashSet::from_iter(v.into_iter())),
            nodes: match (
                response.map_optional(b"nodes", |n| n.as_bytes())?,
                response.map_optional(b"nodes6", |n| n.as_bytes())?,
            ) {
                (None, None) => None,
                (nodes, nodes6) => Some(
                    NodeInfo::from_compact_lists(nodes, nodes6)
                        .or_else(|_| NodeInfo::from_compact_lists(None, nodes))?,
                ),
            },

            interval: response.map_optional(b"interval", |interval| interval.to_integer())?,
            num: response.map_optional(b"num", |num| num.to_integer())?,
            samples: response
                .map_optional(b"samples", |samples| {
                    samples.map_list(|sample| Ok(Hash(sample.to_n_bytes()?)))
                })?
                .map(|s| HashSet::from_iter(s.into_iter())),
        })
    }

    pub fn to_bencode(self, id: NodeId, v6: bool) -> BencodeValue {
        use BencodeValue::*;
        let mut response = HashMap::from([(b"id".into(), ByteString(id.to_vec()))]);
        if let Some(nodes) = self.nodes {
            let nodes4: Vec<_> = nodes.iter().filter_map(|n| n.to_compact_4()).collect();
            let nodes6: Vec<_> = nodes.iter().filter_map(|n| n.to_compact_6()).collect();
            if !nodes4.is_empty() {
                response.insert(
                    b"nodes".into(),
                    ByteString(nodes4.into_iter().flatten().collect()),
                );
            }
            if !nodes6.is_empty() {
                response.insert(
                    b"nodes6".into(),
                    ByteString(nodes6.into_iter().flatten().collect()),
                );
            }
        }
        if let Some(values) = self.values {
            response.insert(
                b"values".to_vec(),
                List(
                    values
                        .iter()
                        .filter_map(|v| match v {
                            SocketAddr::V4(e) if !v6 => Some(ByteString(e.to_compact().into())),
                            SocketAddr::V6(e) if v6 => Some(ByteString(e.to_compact().into())),
                            _ => None,
                        })
                        .collect(),
                ),
            );
        }
        if let Some(token) = self.token {
            response.insert(b"token".into(), ByteString(token));
        }
        Dictionary(response)
    }
}

pub struct DHT {
    id: NodeId,
    socket4: UdpSocket,
    buffer: [u8; 2048],
    queries: HashMap<TransactionId, Query>,
}

impl DHT {
    pub async fn bind(id: NodeId, port: u16) -> Result<Self> {
        Ok(Self {
            id,
            buffer: [0; _],
            socket4: UdpSocket::bind(SocketAddr::new(IpAddr::from(Ipv6Addr::UNSPECIFIED), port))
                .await?,
            queries: HashMap::new(),
        })
    }

    pub async fn receive(&mut self) -> Result<(SocketAddr, Message, Option<Query>)> {
        let (size, addr) = self.socket4.recv_from(&mut self.buffer).await?;
        let mut ret_query = None;
        let message = Message::from_bytes(
            &self.buffer[..size],
            |root, message_type, transaction_id| match message_type {
                'q' => MessageType::from_bencode(root, addr.is_ipv6()),
                'r' | 'e' => match self.queries.remove(transaction_id) {
                    Some(query) => {
                        ret_query = Some(query.clone());
                        MessageType::from_bencode(root, addr.is_ipv6())
                    }
                    .context(format!("Associated with {query:?}")),
                    None => Err(anyhow!("Reply to unsolicited messag")),
                },
                c => Err(anyhow!("Unsupported message type  {c}")),
            },
        )?;
        Ok((addr, message, ret_query))
    }

    pub async fn query(&mut self, addr: &SocketAddr, query: Query) -> Result<()> {
        let transaction_id: [u8; 2] = rng().random();
        self.queries.insert(transaction_id.to_vec(), query.clone());
        self.socket4
            .send_to(
                &Message {
                    transaction_id: transaction_id.to_vec(),
                    client_version: Some(CLIENT_VERSION.to_vec()),
                    type_of: MessageType::Query {
                        id: self.id,
                        query: query.clone(),
                    },
                }
                .to_bytes(addr.is_ipv6()),
                addr,
            )
            .await?;
        Ok(())
    }

    pub async fn respond(
        &self,
        addr: &SocketAddr,
        transaction_id: Vec<u8>,
        response: Response,
    ) -> Result<()> {
        self.socket4
            .send_to(
                &Message {
                    transaction_id,
                    client_version: Some(CLIENT_VERSION.to_vec()),
                    type_of: MessageType::Response {
                        id: self.id,
                        response,
                    },
                }
                .to_bytes(addr.is_ipv6()),
                addr,
            )
            .await?;
        Ok(())
    }
}

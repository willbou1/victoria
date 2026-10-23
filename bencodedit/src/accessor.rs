use std::{array::TryFromSliceError, num::TryFromIntError};

use thiserror::Error;

use super::*;

#[derive(Debug, Clone)]
pub enum BencodePathComponent {
    Index(usize),
    Key(Vec<u8>),
}

impl BencodePathComponent {
    fn string_path(path: &[Self]) -> String {
        let mut ret = path.iter().map(|c| match c {
            Self::Index(index) => format!("[{index}]"),
            Self::Key(key) => format!(".{}", String::from_utf8_lossy(&key)),
        }).collect::<Vec<String>>().join("");
        ret.insert(0, '$');
        ret
    }
}

#[derive(Debug, Error)]
#[error("bencode access error at {}: {kind}\n{tree}", BencodePathComponent::string_path(&path))]
pub struct BencodeAccessError {
    tree: String,
    path: Vec<BencodePathComponent>,
    kind: BencodeAccessErrorKind,
}

#[derive(Debug, Error)]
pub enum BencodeAccessErrorKind {
    #[error("value is not a dictionary")]
    NotADictionary,
    #[error("value is not a list")]
    NotAList,
    #[error("value is not an integer")]
    NotAnInteger,
    #[error("failed to convert integer: {error}")]
    FailedIntegerConversion {
        error: TryFromIntError,
    },
    #[error("value is not a byte string")]
    NotAByteString,
    #[error("invalid byte string length: {error}")]
    InvalidByteStringLength {
        error: TryFromSliceError,
    },
    #[error("invalid utf8: {error}")]
    InvalidUtf8 {
        error: Utf8Error,
    },
    #[error("unable to find key {}", String::from_utf8_lossy(key))]
    KeyNotFound {
        key: Vec<u8>,
    },
    #[error("index {index} is out of bound for an array of length {len}")]
    IndexOutOfBound {
        index: usize,
        len: usize,
    },
}

#[derive(Debug)]
pub struct BencodeAccessor<'a> {
    path: Vec<BencodePathComponent>,
    pub root: &'a BencodeValue,
    pub value: &'a BencodeValue,
}

impl<'a> BencodeAccessor<'a> {
    pub fn new(value: &'a BencodeValue) -> Self {
        Self {
            path: vec![],
            value,
            root: value,
        }
    }

    pub fn at(&self, index: usize) -> Result<BencodeAccessor<'a>, BencodeAccessError> {
        let Some(list) = self.value.as_list() else {
            return Err(BencodeAccessError {
                tree: format!("{:?}", self.root),
                path: self.path.to_vec(),
                kind: BencodeAccessErrorKind::NotAList,
            });
        };
        let Some(value) = list.get(index) else {
            return Err(BencodeAccessError {
                tree: format!("{:?}", self.root),
                path: self.path.to_vec(),
                kind: BencodeAccessErrorKind::IndexOutOfBound { index, len: list.len() },
            });
        };
        let mut path = self.path.clone();
        path.push(BencodePathComponent::Index(index));
        Ok(BencodeAccessor { path, value, ..*self })
    }

    pub fn required(&self, key: &[u8]) -> Result<BencodeAccessor<'a>, BencodeAccessError> {
        let Some(accessor) = self.optional(key)? else {
            return Err(BencodeAccessError {
                tree: format!("{:?}", self.root),
                path: self.path.to_vec(),
                kind: BencodeAccessErrorKind::KeyNotFound { key: key.to_vec() },
            });
        };
        Ok(accessor)
    }

    pub fn map_optional<T>(
        &self,
        key: &[u8],
        f: impl FnOnce(BencodeAccessor<'a>) -> Result<T, BencodeAccessError>,
    ) -> Result<Option<T>, BencodeAccessError> {
        self.optional(key)?.map(f).transpose()
    }

    pub fn optional(&self, key: &[u8]) -> Result<Option<BencodeAccessor<'a>>, BencodeAccessError> {
        let Some(dict) = self.value.as_dict() else {
            return Err(BencodeAccessError {
                tree: format!("{:?}", self.root),
                path: self.path.to_vec(),
                kind: BencodeAccessErrorKind::NotADictionary,
            });
        };
        let Some(value) = dict.get(key) else {
            return Ok(None);
        };
        let mut path = self.path.clone();
        path.push(BencodePathComponent::Key(key.to_vec()));
        Ok(Some(BencodeAccessor { path, value, ..*self }))
    }

    pub fn map_dict<F, T>(&self, f: F) -> Result<Vec<T>, BencodeAccessError>
        where F: Fn(&[u8], BencodeAccessor<'a>) -> Result<T, BencodeAccessError>
    {
        let Some(dict) = self.value.as_dict() else {
            return Err(BencodeAccessError {
                tree: format!("{:?}", self.root),
                path: self.path.to_vec(),
                kind: BencodeAccessErrorKind::NotADictionary,
            });
        };
        dict.iter()
            .map(|(key, value)| {
                let mut path = self.path.clone();
                path.push(BencodePathComponent::Key(key.to_vec()));
                f(key, BencodeAccessor {path, value, ..*self}) 
            })
            .collect::<Result<Vec<T>, BencodeAccessError>>()
    }

    pub fn map_list<F, T>(&self, f: F) -> Result<Vec<T>, BencodeAccessError>
        where F: Fn(BencodeAccessor<'a>) -> Result<T, BencodeAccessError>
    {
        let Some(list) = self.value.as_list() else {
            return Err(BencodeAccessError {
                tree: format!("{:?}", self.root),
                path: self.path.to_vec(),
                kind: BencodeAccessErrorKind::NotAList,
            });
        };
        list.iter().enumerate()
            .map(|(i, value)| {
                let mut path = self.path.clone();
                path.push(BencodePathComponent::Index(i));
                f(BencodeAccessor {path, value, ..*self}) 
            })
            .collect::<Result<Vec<T>, BencodeAccessError>>()
    }

    pub fn as_n_bytes<const N: usize>(&self) -> Result<&'a [u8; N], BencodeAccessError> {
        self.as_bytes()?.try_into().map_err(|error| BencodeAccessError {
            tree: format!("{:?}", self.root),
            path: self.path.clone(),
            kind: BencodeAccessErrorKind::InvalidByteStringLength { error },
        })
    }

    pub fn as_bytes(&self) -> Result<&'a [u8], BencodeAccessError> {
        self.value.as_bytes().ok_or(BencodeAccessError {
            tree: format!("{:?}", self.root),
            path: self.path.clone(),
            kind: BencodeAccessErrorKind::NotAByteString,
        })
    }

    pub fn as_str(&self) -> Result<&'a str, BencodeAccessError> {
        str::from_utf8(self.as_bytes()?).map_err(|error| BencodeAccessError {
            tree: format!("{:?}", self.root),
            path: self.path.clone(),
            kind: BencodeAccessErrorKind::InvalidUtf8 { error },
        })
    }

    pub fn as_str_list(&self) -> Result<Vec<&'a str>, BencodeAccessError> {
        self.map_list(|s| s.as_str())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, BencodeAccessError> {
        self.as_bytes().map(|s| s.to_vec())
    }

    pub fn to_n_bytes<const N: usize>(&self) -> Result<[u8; N], BencodeAccessError> {
        self.as_n_bytes().map(|s| s.to_owned())
    }

    pub fn to_string(&self) -> Result<String, BencodeAccessError> {
        self.as_str().map(|s| s.to_string())
    }

    pub fn to_string_list(&self) -> Result<Vec<String>, BencodeAccessError> {
        self.map_list(|s| s.to_string())
    }

    pub fn to_i64(&self) -> Result<i64, BencodeAccessError> {
        self.value.as_i64().ok_or(BencodeAccessError {
            tree: format!("{:?}", self.root),
            path: self.path.clone(),
            kind: BencodeAccessErrorKind::NotAnInteger,
        })
    }

    pub fn to_integer<I>(&self) -> Result<I, BencodeAccessError>
    where I: TryFrom<i64, Error = TryFromIntError>
    {
        I::try_from(self.to_i64()?).map_err(|error| BencodeAccessError {
            tree: format!("{:?}", self.root),
            path: self.path.clone(),
            kind: BencodeAccessErrorKind::FailedIntegerConversion { error },
        })
    }

    pub fn to_bool(&self) -> Result<bool, BencodeAccessError> {
        self.to_i64().map(|i| i != 0)
    }
}

pub mod accessor;

use thiserror::Error;
use std::{
    collections::HashMap, fmt, num::ParseIntError, str::Utf8Error
};

#[derive(Debug, Error)]
#[error("Bencode error at {position}: {kind}\n{}", String::from_utf8_lossy(encoded))]
pub struct BencodeError {
    pub encoded: Vec<u8>,
    pub position: usize,
    pub kind: BencodeErrorKind,
}

#[derive(Debug, Error)]
pub enum BencodeErrorKind {
    #[error("no bencode value")]
    Empty,
    
    #[error("invalid value starting with {byte}")]
    InvalidValue {
        byte: u8,
    },

    #[error("unterminated dictionary")]
    UnterminatedDictionary,
    #[error("a dictionary key should be a byte string")]
    DictionaryKeyIsNotAByteString,
    #[error("dictionary key '{}' has no associated value", String::from_utf8_lossy(key))]
    DictionaryKeyHasNoValue {
        key: Vec<u8>,
    },

    #[error("unterminated list")]
    UnterminatedList,

    #[error("no semicolon in byte string")]
    MissingColonInByteString,
    #[error("invalid byte string length: {error}")]
    InvalidByteStringLength {
        error: ParseIntError,
    },
    #[error("byte string length must be valid UTF-8: {error}")]
    InvalidUtf8ByteStringLength {
        error: Utf8Error,
    },
    #[error("expected byte string of length {expected}, but got {got}")]
    InsufficientByteStringLength {
        expected: usize,
        got: usize,
    },

    #[error("unterminated integer")]
    UnterminatedInteger,
    #[error("integer must be valid UTF-8: {error}")]
    InvalidUtf8Integer {
        error: Utf8Error,
    },
    #[error("invalid integer: {error}")]
    InvalidInteger {
        error: ParseIntError,
    },
}

#[derive(Clone)]
pub enum BencodeValue {
    ByteString(Vec<u8>),
    Integer(i64),
    List(Vec<BencodeValue>),
    Dictionary(HashMap<Vec<u8>, BencodeValue>),
}

impl fmt::Debug for BencodeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteString(bytes) => {
                write!(f, "ByteString({:?})", String::from_utf8_lossy(bytes))
            }
            Self::Integer(value) => {
                f.debug_tuple("Integer").field(value).finish()
            }
            Self::List(values) => {
                f.debug_tuple("List").field(values).finish()
            }
            Self::Dictionary(dict) => {
                let dict: HashMap<_, _> = dict
                    .iter()
                    .map(|(key, value)| {
                        (String::from_utf8_lossy(key), value)
                    })
                    .collect();

                f.debug_tuple("Dictionary").field(&dict).finish()
            }
        }
    }
}

impl BencodeValue {
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::ByteString(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        str::from_utf8(self.as_bytes()?).ok()
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[BencodeValue]> {
        match self {
            Self::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&HashMap<Vec<u8>, BencodeValue>> {
        match self {
            Self::Dictionary(d) => Some(d),
            _ => None,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            BencodeValue::ByteString(byte_string) => {
                let mut encoded = byte_string.len().to_string().into_bytes();
                encoded.push(b':');
                encoded.extend(byte_string);
                encoded
            },
            BencodeValue::Integer(integer) => {
                let mut encoded = vec![b'i'];
                encoded.extend(integer.to_string().bytes());
                encoded.push(b'e');
                encoded
            },
            BencodeValue::List(list) => {
            let mut encoded = vec![b'l'];
                encoded.extend(
                    list.iter().flat_map(Self::to_bytes)
                );
                encoded.push(b'e');
                encoded 
            }
            BencodeValue::Dictionary(dic) => {
                let mut encoded = vec![b'd'];
                let mut entries: Vec<_> = dic.iter().collect();
                entries.sort_by_key(|(key, _)| *key);
                for (key, val) in entries {
                    encoded.extend(
                        BencodeValue::ByteString(key.to_vec()).to_bytes()
                    );
                    encoded.extend(val.to_bytes());
                }
                encoded.push(b'e');
                encoded
            },
        }
    }

    pub fn from_dictionary_bytes(
        encoded: &[u8],
        mut position: usize,
   ) -> Result<(Self, usize), BencodeError> {
       use BencodeErrorKind::*;
       let mut key_vals = HashMap::new();
       let start = position;

       loop {
           if position >= encoded.len() {
               return Err(BencodeError {
                   encoded: encoded.to_vec(),
                   position: start,
                   kind: UnterminatedDictionary,
               });
           }
           if encoded[position] == b'e' {
               return Ok((Self::Dictionary(key_vals), position + 1));
           }
           
           let (key, val_pos) = Self::from_bytes_internal(encoded, position)?.unwrap();
           let Self::ByteString(key) = key else {
               return Err(BencodeError {
                   encoded: encoded.to_vec(),
                   position,
                   kind: DictionaryKeyIsNotAByteString,
               });
           };
           let Some((value, next_position)) =
               Self::from_bytes_internal(encoded, val_pos)?
               else {
                   return Err(BencodeError {
                       encoded: encoded.to_vec(),
                       position: position,
                       kind: DictionaryKeyHasNoValue { key },
                   });
               };

           key_vals.insert(key, value);
           position = next_position;
       }
    }

    pub fn from_list_bytes(
        encoded: &[u8],
        mut position: usize,
   ) -> Result<(Self, usize), BencodeError> {
       use BencodeErrorKind::*;
       let mut list = vec![];
       let start = position;

       loop {
           if position >= encoded.len() {
               return Err(BencodeError {
                   encoded: encoded.to_vec(),
                   position: start,
                   kind: UnterminatedList,
               });
           }
           if encoded[position] == b'e' {
               return Ok((Self::List(list), position + 1));
           }
           
           let (val, next_position) = Self::from_bytes_internal(encoded, position)?.unwrap();
           list.push(val);

           position = next_position;
       }
    }

    pub fn from_bytes(encoded: &[u8]) -> Result<(Self, &[u8]), BencodeError> {
        let Some((val, next_position)) = Self::from_bytes_internal(encoded, 0)? else {
            return Err(BencodeError {
                encoded: encoded.to_vec(),
                position: 0,
                kind: BencodeErrorKind::Empty,
            });
        };
        Ok((val, &encoded[next_position..]))
    }

    fn from_bytes_internal(encoded: &[u8], position: usize) -> Result<Option<(Self, usize)>, BencodeError> {
        if position >= encoded.len() {
            return Ok(None);
        }
        use BencodeErrorKind::*;

        match encoded[position] {
            b'd' => Self::from_dictionary_bytes(encoded, position + 1).map(|r| Some(r)),
            b'l' => Self::from_list_bytes(encoded, position + 1).map(|r| Some(r)),

            b'i' => {
                let e_index = encoded[position..]
                    .iter()
                    .position(|&byte| byte == b'e')
                    .ok_or_else(|| BencodeError {
                        encoded: encoded.to_vec(),
                        position,
                        kind: UnterminatedInteger,
                    })? + position;

                let integer_bytes = &encoded[(position + 1)..e_index];

                let integer_string = str::from_utf8(integer_bytes)
                    .map_err(|error| BencodeError {
                        encoded: encoded.to_vec(),
                        position,
                        kind: InvalidUtf8Integer { error },
                    })?;

                let integer = integer_string.parse::<i64>()
                    .map_err(|error| BencodeError {
                        encoded: encoded.to_vec(),
                        position,
                        kind: InvalidInteger { error },
                    })?;

                Ok(Some((Self::Integer(integer), e_index + 1)))
            }

            _ if encoded[position].is_ascii_digit() => {
                let colon_index = encoded[position..]
                    .iter()
                    .position(|&byte| byte == b':')
                    .ok_or_else(|| BencodeError {
                        encoded: encoded.to_vec(),
                        position,
                        kind: MissingColonInByteString,
                    })? + position;

                let number_bytes = &encoded[position..colon_index];

                let number_string = str::from_utf8(number_bytes)
                    .map_err(|error| BencodeError {
                        encoded: encoded.to_vec(),
                        position,
                        kind: InvalidUtf8ByteStringLength { error },
                    })?;

                let number = number_string.parse::<usize>()
                    .map_err(|error| BencodeError {
                        encoded: encoded.to_vec(),
                        position,
                        kind: InvalidByteStringLength { error },
                    })?;

                let string_start = colon_index + 1;
                let string_end = string_start + number;

                if encoded.len() < string_end {
                    return Err(BencodeError {
                        encoded: encoded.to_vec(),
                        position,
                        kind: InsufficientByteStringLength {
                            expected: number,
                            got: encoded.len() - string_start,
                        },
                    });
                }

                let string = &encoded[string_start..string_end];
                Ok(Some((Self::ByteString(string.to_vec()), string_end)))
            }

            _ => Err(BencodeError {
                encoded: encoded.to_vec(),
                position,
                kind: InvalidValue { byte: encoded[position] },
            }),
        }
    }
}


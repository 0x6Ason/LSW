// SPDX-License-Identifier: GPL-3.0-or-later

use crate::codec::{push_string, Decoder};
use crate::{ProtocolError as LswError, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePutRequest {
    pub destination: String,
    pub length: u64,
}

impl FilePutRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = self.length.to_be_bytes().to_vec();
        push_string(&mut payload, &self.destination)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let length = decoder.u64()?;
        let destination = decoder.string()?;
        decoder.finish()?;
        if destination.is_empty() {
            return Err(LswError::Protocol(
                "file destination must not be empty".to_owned(),
            ));
        }
        Ok(Self {
            destination,
            length,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileGetRequest {
    pub source: String,
}

impl FileGetRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        push_string(&mut payload, &self.source)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let source = decoder.string()?;
        decoder.finish()?;
        if source.is_empty() {
            return Err(LswError::Protocol(
                "file source must not be empty".to_owned(),
            ));
        }
        Ok(Self { source })
    }
}

pub fn encode_file_length(length: u64) -> Vec<u8> {
    length.to_be_bytes().to_vec()
}

pub fn decode_file_length(payload: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = payload
        .try_into()
        .map_err(|_| LswError::Protocol("file length must contain eight bytes".to_owned()))?;
    Ok(u64::from_be_bytes(bytes))
}

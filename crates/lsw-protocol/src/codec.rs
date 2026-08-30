// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{ProtocolError as LswError, Result, MAX_ARGUMENTS, MAX_STRING_BYTES};

pub fn constant_time_token_eq(left: &str, right: &str) -> bool {
    let mut difference = left.len() ^ right.len();
    let maximum = left.len().max(right.len());
    for index in 0..maximum {
        let left_byte = left.as_bytes().get(index).copied().unwrap_or_default();
        let right_byte = right.as_bytes().get(index).copied().unwrap_or_default();
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

pub(crate) fn push_strings(payload: &mut Vec<u8>, values: &[String]) -> Result<()> {
    let count = u16::try_from(values.len())
        .map_err(|_| LswError::Protocol("too many string values".to_owned()))?;
    if values.len() > MAX_ARGUMENTS {
        return Err(LswError::Protocol(format!(
            "more than {MAX_ARGUMENTS} string values"
        )));
    }
    payload.extend_from_slice(&count.to_be_bytes());
    for value in values {
        push_string(payload, value)?;
    }
    Ok(())
}

pub(crate) fn push_string(payload: &mut Vec<u8>, value: &str) -> Result<()> {
    if value.len() > MAX_STRING_BYTES {
        return Err(LswError::Protocol(format!(
            "string exceeds the {MAX_STRING_BYTES} byte limit"
        )));
    }
    let length = u32::try_from(value.len())
        .map_err(|_| LswError::Protocol("string length does not fit in u32".to_owned()))?;
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(value.as_bytes());
    Ok(())
}

pub(crate) struct Decoder<'a> {
    pub(crate) remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(payload: &'a [u8]) -> Self {
        Self { remaining: payload }
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        let value = *self
            .remaining
            .first()
            .ok_or_else(|| LswError::Protocol("truncated u8 field".to_owned()))?;
        self.remaining = &self.remaining[1..];
        Ok(value)
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes(
            bytes.try_into().expect("fixed u32 field length"),
        ))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes(
            bytes.try_into().expect("fixed u64 field length"),
        ))
    }

    pub(crate) fn i16(&mut self) -> Result<i16> {
        let bytes = self.take(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub(crate) fn string(&mut self) -> Result<String> {
        let length = usize::try_from(self.u32()?)
            .map_err(|_| LswError::Protocol("invalid string length".to_owned()))?;
        if length > MAX_STRING_BYTES {
            return Err(LswError::Protocol(format!(
                "string exceeds the {MAX_STRING_BYTES} byte limit"
            )));
        }
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| LswError::Protocol("string is not valid UTF-8".to_owned()))
    }

    pub(crate) fn strings(&mut self) -> Result<Vec<String>> {
        let count = usize::from(self.u16()?);
        if count > MAX_ARGUMENTS {
            return Err(LswError::Protocol(format!(
                "more than {MAX_ARGUMENTS} string values"
            )));
        }
        (0..count).map(|_| self.string()).collect()
    }

    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        if self.remaining.len() < length {
            return Err(LswError::Protocol("truncated payload".to_owned()));
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    pub(crate) fn finish(self) -> Result<()> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(LswError::Protocol("payload has trailing bytes".to_owned()))
        }
    }
}

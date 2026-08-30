// SPDX-License-Identifier: GPL-3.0-or-later

use crate::codec::{push_string, Decoder};
use crate::{ProtocolError as LswError, Result, WindowsUserRole};

pub struct UserCreateRequest {
    pub user_name: String,
    pub password: Vec<u8>,
    pub administrator: bool,
}

impl std::fmt::Debug for UserCreateRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UserCreateRequest")
            .field("user_name", &self.user_name)
            .field("password", &"[REDACTED]")
            .field("administrator", &self.administrator)
            .finish()
    }
}

impl Drop for UserCreateRequest {
    fn drop(&mut self) {
        self.password.fill(0);
    }
}

impl UserCreateRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        crate::validate_windows_user_name(&self.user_name)?;
        validate_password(&self.password)?;
        let mut payload = Vec::new();
        push_string(&mut payload, &self.user_name)?;
        let length = u32::try_from(self.password.len())
            .map_err(|_| LswError::Protocol("password is too long".to_owned()))?;
        payload.extend_from_slice(&length.to_be_bytes());
        payload.extend_from_slice(&self.password);
        payload.push(u8::from(self.administrator));
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let user_name = decoder.string()?;
        let length = usize::try_from(decoder.u32()?)
            .map_err(|_| LswError::Protocol("invalid password length".to_owned()))?;
        if length > 1024 {
            return Err(LswError::Protocol(
                "password exceeds the 1024 byte protocol limit".to_owned(),
            ));
        }
        let mut password = decoder.take(length)?.to_vec();
        let flag = match decoder.u8() {
            Ok(flag) => flag,
            Err(error) => {
                password.fill(0);
                return Err(error);
            }
        };
        let administrator = match flag {
            0 => false,
            1 => true,
            _ => {
                password.fill(0);
                return Err(LswError::Protocol(
                    "administrator flag must be zero or one".to_owned(),
                ));
            }
        };
        if let Err(error) = decoder.finish() {
            password.fill(0);
            return Err(error);
        }
        let request = Self {
            user_name,
            password,
            administrator,
        };
        crate::validate_windows_user_name(&request.user_name)?;
        validate_password(&request.password)?;
        Ok(request)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserSetRoleRequest {
    pub user_name: String,
    pub role: WindowsUserRole,
}

impl UserSetRoleRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        crate::validate_windows_user_name(&self.user_name)?;
        let mut payload = Vec::new();
        push_string(&mut payload, &self.user_name)?;
        payload.push(match self.role {
            WindowsUserRole::Standard => 0,
            WindowsUserRole::Administrator => 1,
        });
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let user_name = decoder.string()?;
        let role = match decoder.u8()? {
            0 => WindowsUserRole::Standard,
            1 => WindowsUserRole::Administrator,
            _ => {
                return Err(LswError::Protocol(
                    "Windows user role must be zero or one".to_owned(),
                ))
            }
        };
        decoder.finish()?;
        crate::validate_windows_user_name(&user_name)?;
        Ok(Self { user_name, role })
    }
}

fn validate_password(password: &[u8]) -> Result<()> {
    if password.is_empty() {
        return Err(LswError::Protocol("password must not be empty".to_owned()));
    }
    if password.len() > 1024 {
        return Err(LswError::Protocol(
            "password exceeds the 1024 byte protocol limit".to_owned(),
        ));
    }
    let password = std::str::from_utf8(password)
        .map_err(|_| LswError::Protocol("password must be valid UTF-8".to_owned()))?;
    if password.contains('\0') || password.encode_utf16().count() > 256 {
        return Err(LswError::Protocol(
            "password must contain at most 256 UTF-16 code units and no NUL".to_owned(),
        ));
    }
    Ok(())
}

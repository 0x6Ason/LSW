// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{HostCapabilities, LswError, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsEdition {
    pub index: u32,
    pub name: String,
    pub edition_id: Option<String>,
}

impl WindowsEdition {
    pub fn matches(&self, requested: &str) -> bool {
        let requested = normalized_edition(requested);
        let name = normalized_edition(&self.name);
        let edition_id = self.edition_id.as_deref().map(normalized_edition);
        name == requested
            || edition_id.as_deref() == Some(requested.as_str())
            || (requested == "pro"
                && (edition_id.as_deref() == Some("professional") || name.ends_with("pro")))
            || (requested == "home" && name.ends_with("home"))
            || (requested == "enterprise" && name.ends_with("enterprise"))
            || (requested == "education" && name.ends_with("education"))
    }
}

pub struct WindowsMediaInspector {
    capabilities: HostCapabilities,
}

impl WindowsMediaInspector {
    pub fn new(capabilities: HostCapabilities) -> Self {
        Self { capabilities }
    }

    pub fn inspect(&self, iso: &Path, scratch_root: &Path) -> Result<Vec<WindowsEdition>> {
        if !iso.is_file() {
            return Err(LswError::InvalidValue {
                field: "source ISO",
                reason: format!("{} is not a regular file", iso.display()),
            });
        }
        let xorriso = self
            .capabilities
            .xorriso
            .as_ref()
            .ok_or_else(|| LswError::MissingCapabilities(vec!["xorriso"]))?;
        let wimlib = self
            .capabilities
            .wimlib_imagex
            .as_ref()
            .ok_or_else(|| LswError::MissingCapabilities(vec!["wimlib-imagex"]))?;

        fs::create_dir_all(scratch_root)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let temporary =
            scratch_root.join(format!("media-inspection-{}-{nonce}", std::process::id()));
        fs::create_dir(&temporary)?;
        set_private_directory_permissions(&temporary)?;
        let temporary = TemporaryDirectory(temporary);

        let mut extracted = None;
        let mut errors = Vec::new();
        for (source, filename) in [
            ("/sources/install.wim", "install.wim"),
            ("/sources/install.esd", "install.esd"),
        ] {
            let destination = temporary.0.join(filename);
            let output = Command::new(xorriso)
                .args(["-osirrox", "on", "-indev"])
                .arg(iso)
                .args(["-extract", source])
                .arg(&destination)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .output()?;
            if output.status.success() && destination.is_file() {
                extracted = Some(destination);
                break;
            }
            errors.push(String::from_utf8_lossy(&output.stderr).trim().to_owned());
            let _ = fs::remove_file(destination);
        }
        let extracted = extracted.ok_or_else(|| LswError::InvalidValue {
            field: "Windows installation media",
            reason: format!(
                "could not extract sources/install.wim or sources/install.esd with xorriso{}",
                compact_errors(&errors)
            ),
        })?;

        let output = Command::new(wimlib)
            .arg("info")
            .arg(&extracted)
            .arg("--xml")
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(LswError::InvalidValue {
                field: "Windows installation media",
                reason: format!(
                    "wimlib-imagex could not inspect the install image: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        let xml = decode_xml(&output.stdout)?;
        let editions = parse_editions(&xml)?;
        if editions.is_empty() {
            return Err(LswError::InvalidValue {
                field: "Windows installation media",
                reason: "the install image did not contain a named Windows edition".to_owned(),
            });
        }
        Ok(editions)
    }
}

struct TemporaryDirectory(PathBuf);

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn parse_editions(xml: &str) -> Result<Vec<WindowsEdition>> {
    let mut editions = Vec::new();
    let mut remaining = xml;
    while let Some(start) = remaining.find("<IMAGE") {
        remaining = &remaining[start..];
        let Some(header_end) = remaining.find('>') else {
            break;
        };
        let header = &remaining[..=header_end];
        let Some(block_end) = remaining.find("</IMAGE>") else {
            break;
        };
        let block = &remaining[..block_end];
        remaining = &remaining[block_end + "</IMAGE>".len()..];

        let index = xml_attribute(header, "INDEX")
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| LswError::InvalidValue {
                field: "Windows installation media",
                reason: "an install image has no valid INDEX attribute".to_owned(),
            })?;
        let Some(name) = xml_element(block, "NAME") else {
            continue;
        };
        editions.push(WindowsEdition {
            index,
            name: xml_unescape(name.trim()),
            edition_id: xml_element(block, "EDITIONID")
                .map(|value| xml_unescape(value.trim()))
                .filter(|value| !value.is_empty()),
        });
    }
    Ok(editions)
}

fn xml_attribute<'a>(element: &'a str, attribute: &str) -> Option<&'a str> {
    let needle = format!("{attribute}=\"");
    let value = element.split_once(&needle)?.1;
    Some(value.split_once('"')?.0)
}

fn xml_element<'a>(block: &'a str, element: &str) -> Option<&'a str> {
    let open = format!("<{element}>");
    let close = format!("</{element}>");
    let value = block.split_once(&open)?.1;
    Some(value.split_once(&close)?.0)
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn normalized_edition(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect()
}

fn decode_xml(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).map_err(|error| LswError::InvalidValue {
            field: "Windows installation media",
            reason: format!("WIM XML is not valid UTF-16LE: {error}"),
        });
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).map_err(|error| LswError::InvalidValue {
            field: "Windows installation media",
            reason: format!("WIM XML is not valid UTF-16BE: {error}"),
        });
    }
    String::from_utf8(bytes.to_vec()).map_err(|error| LswError::InvalidValue {
        field: "Windows installation media",
        reason: format!("WIM XML is not valid UTF-8: {error}"),
    })
}

fn compact_errors(errors: &[String]) -> String {
    let messages = errors
        .iter()
        .filter(|error| !error.is_empty())
        .map(|error| error.lines().last().unwrap_or(error))
        .collect::<Vec<_>>();
    if messages.is_empty() {
        String::new()
    } else {
        format!(": {}", messages.join("; "))
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wim_xml_and_matches_friendly_edition_aliases() {
        let xml = r#"<WIM><IMAGE INDEX="1"><NAME>Windows 11 Home</NAME><WINDOWS><EDITIONID>Core</EDITIONID></WINDOWS></IMAGE><IMAGE INDEX="6"><NAME>Windows 11 Pro</NAME><WINDOWS><EDITIONID>Professional</EDITIONID></WINDOWS></IMAGE></WIM>"#;
        let editions = parse_editions(xml).expect("fixture should parse");
        assert_eq!(editions.len(), 2);
        assert_eq!(editions[1].index, 6);
        assert!(editions[1].matches("pro"));
        assert!(editions[1].matches("Professional"));
        assert!(editions[0].matches("Windows 11 Home"));
    }

    #[test]
    fn decodes_utf16_wim_xml() {
        let mut bytes = vec![0xff, 0xfe];
        for unit in "<WIM/>".encode_utf16() {
            bytes.extend(unit.to_le_bytes());
        }
        assert_eq!(decode_xml(&bytes).expect("UTF-16 should decode"), "<WIM/>");
    }
}

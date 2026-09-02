// SPDX-License-Identifier: GPL-3.0-or-later

//! Direct Microsoft Windows ISO resolution and bounded resumable downloads.
//!
//! Signed CDN URLs are treated as secrets: only [`SecretDownloadUrl::expose`]
//! may reveal one, while debug and error paths use the redacted representation.
//! Every completed ISO is verified against Microsoft's published SHA-256 before
//! it is promoted to the caller-visible destination.

#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{HostCapabilities, LswError, Result};

const MICROSOFT_DOWNLOAD_PAGE: &str = "https://www.microsoft.com/en-us/software-download/windows11";
const MICROSOFT_SKU_ENDPOINT: &str =
    "https://www.microsoft.com/software-download-connector/api/getskuinformationbyproductedition";
const MICROSOFT_LINK_ENDPOINT: &str =
    "https://www.microsoft.com/software-download-connector/api/GetProductDownloadLinksBySku";
const MICROSOFT_TAGS_ENDPOINT: &str = "https://vlscppe.microsoft.com/tags";
const MICROSOFT_MDT_SCRIPT: &str = "https://ov-df.microsoft.com/mdt.js";
const MICROSOFT_MDT_ENDPOINT: &str = "https://ov-df.microsoft.com/";
const MICROSOFT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";
const MICROSOFT_PROFILE: &str = "606624d44113";
const MICROSOFT_LOCALE: &str = "en-US";
const MICROSOFT_ORG_ID: &str = "y6jn8c31";
const MICROSOFT_CUSTOMER_ID: &str = "560dc9f3-1aa5-4a2f-b63c-9e18f8d0e175";
const MAX_MICROSOFT_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DOWNLOAD_ATTEMPTS: usize = 3;
const MAX_CDN_CONNECTIONS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
/// Parameters that select an official Windows ISO.
pub struct MicrosoftIsoRequest {
    /// Microsoft display language, such as `English` or `French`.
    pub language: String,
}

impl Default for MicrosoftIsoRequest {
    fn default() -> Self {
        Self {
            language: "English".to_owned(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
/// An allowlisted Microsoft CDN URL whose query contains a bearer-like token.
pub struct SecretDownloadUrl(Url);

impl SecretDownloadUrl {
    fn parse(value: &str) -> Result<Self> {
        let url = Url::parse(value).map_err(|_| LswError::InvalidValue {
            field: "Microsoft download URL",
            reason: "Microsoft returned an invalid URL".to_owned(),
        })?;
        validate_microsoft_cdn_url(&url)?;
        Ok(Self(url))
    }

    /// Returns the full signed URL for the downloader only.
    ///
    /// Callers must not log, persist, or include this value in diagnostics.
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    /// Returns a safe display form with query and fragment data removed.
    pub fn redacted(&self) -> String {
        let mut url = self.0.clone();
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    }
}

impl fmt::Debug for SecretDownloadUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SecretDownloadUrl")
            .field(&format_args!("{}?<redacted>", self.redacted()))
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A resolved ISO identity and its short-lived official download location.
pub struct ResolvedWindowsIso {
    /// Microsoft download-page product identifier.
    pub product_id: String,
    /// Microsoft language SKU identifier.
    pub sku_id: String,
    /// Canonical Microsoft language label.
    pub language: String,
    /// Selected media architecture; the current release line requires `x64`.
    pub architecture: String,
    /// Filename derived from the allowlisted CDN path.
    pub filename: String,
    /// SHA-256 published by Microsoft for this exact language and architecture.
    pub expected_sha256: String,
    /// Microsoft-provided signed-link expiration, when present.
    pub expires_at: Option<String>,
    /// Validated signed Microsoft CDN URL.
    pub download_url: SecretDownloadUrl,
}

#[derive(Clone, Debug)]
/// Resolves current Windows 11 media through Microsoft's public session flow.
pub struct MicrosoftIsoResolver {
    agent: ureq::Agent,
}

impl Default for MicrosoftIsoResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrosoftIsoResolver {
    /// Creates a resolver with bounded timeouts and redirects disabled.
    pub fn new() -> Self {
        Self {
            agent: http_agent(),
        }
    }

    /// Reads Microsoft's current published SHA-256 for one x64 language.
    ///
    /// This does not request a short-lived download URL, so callers that
    /// already have an ISO can verify it without consuming a Microsoft
    /// download session.
    pub fn published_sha256(&self, request: &MicrosoftIsoRequest) -> Result<String> {
        validate_language(&request.language)?;
        self.fetch_catalog()?.hash_for(&request.language, "x64")
    }

    /// Resolves one x64 ISO and its Microsoft-published SHA-256.
    pub fn resolve(&self, request: &MicrosoftIsoRequest) -> Result<ResolvedWindowsIso> {
        validate_language(&request.language)?;
        let catalog = self.fetch_catalog()?;
        let mut session = MicrosoftSession::new(self.agent.clone())?;
        session.register()?;
        let languages = session.fetch_languages(&catalog.product_id)?;
        let language = select_language(&languages, &request.language)?;
        let response = session.fetch_download_links(&catalog.product_id, &language.id)?;
        let option = response
            .options
            .into_iter()
            .find(|option| option.architecture().eq_ignore_ascii_case("x64"))
            .ok_or_else(|| LswError::InvalidValue {
                field: "Microsoft ISO response",
                reason: "Microsoft returned no x64 download option".to_owned(),
            })?;
        let download_url = SecretDownloadUrl::parse(&option.uri)?;
        let filename = iso_filename(&download_url.0)?;
        let expected_sha256 = catalog.hash_for(&language.language, "x64")?;

        Ok(ResolvedWindowsIso {
            product_id: catalog.product_id,
            sku_id: language.id,
            language: language.language,
            architecture: "x64".to_owned(),
            filename,
            expected_sha256,
            expires_at: response.expiration,
            download_url,
        })
    }

    fn fetch_catalog(&self) -> Result<DownloadPageCatalog> {
        let page = read_response(
            self.agent
                .get(MICROSOFT_DOWNLOAD_PAGE)
                .set("User-Agent", MICROSOFT_USER_AGENT)
                .set("Accept", "text/html,application/xhtml+xml")
                .call()
                .map_err(|error| http_error("fetching the Microsoft download page", error))?,
            "Microsoft download page",
        )?;
        parse_download_page(&page)
    }
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        // Session cookies and identifiers must never follow an implicit
        // cross-origin redirect. Every Microsoft API origin is allowlisted
        // explicitly; an unexpected redirect fails closed.
        .redirects(0)
        .build()
}

struct MicrosoftSession {
    agent: ureq::Agent,
    session_id: String,
    cookies: BTreeMap<String, String>,
}

impl MicrosoftSession {
    fn new(agent: ureq::Agent) -> Result<Self> {
        Ok(Self {
            agent,
            session_id: new_session_id()?,
            cookies: BTreeMap::new(),
        })
    }

    fn register(&mut self) -> Result<()> {
        let tags = query_url(
            MICROSOFT_TAGS_ENDPOINT,
            &[
                ("org_id", MICROSOFT_ORG_ID),
                ("session_id", &self.session_id),
            ],
        )?;
        let _ = self.get(&tags, None, "registering the Microsoft session")?;

        let script = query_url(
            MICROSOFT_MDT_SCRIPT,
            &[
                ("instanceId", MICROSOFT_CUSTOMER_ID),
                ("PageId", "si"),
                ("session_id", &self.session_id),
            ],
        )?;
        let body = self.get(&script, None, "fetching the Microsoft session script")?;
        if let (Some(w), Some(rticks)) = (extract_mdt_w(&body), extract_rticks(&body)) {
            let now = unix_time_millis();
            let endpoint = query_url(
                MICROSOFT_MDT_ENDPOINT,
                &[
                    ("session_id", &self.session_id),
                    ("CustomerId", MICROSOFT_CUSTOMER_ID),
                    ("PageId", "si"),
                    ("w", &w),
                    ("mdt", &now),
                    ("rticks", &rticks),
                ],
            )?;
            let _ = self.get(&endpoint, None, "completing the Microsoft session")?;
        }
        Ok(())
    }

    fn fetch_languages(&mut self, product_id: &str) -> Result<Vec<MicrosoftLanguage>> {
        let url = query_url(
            MICROSOFT_SKU_ENDPOINT,
            &[
                ("profile", MICROSOFT_PROFILE),
                ("productEditionId", product_id),
                ("SKU", "undefined"),
                ("friendlyFileName", "undefined"),
                ("Locale", MICROSOFT_LOCALE),
                ("sessionID", &self.session_id),
            ],
        )?;
        let raw = self.get(
            &url,
            Some(MICROSOFT_DOWNLOAD_PAGE),
            "fetching Windows ISO languages",
        )?;
        parse_languages(&raw)
    }

    fn fetch_download_links(
        &mut self,
        product_id: &str,
        sku_id: &str,
    ) -> Result<MicrosoftDownloadResponse> {
        // Microsoft expects the SKU request to be preceded by a product query
        // in the same session.
        let _ = self.fetch_languages(product_id)?;
        let url = query_url(
            MICROSOFT_LINK_ENDPOINT,
            &[
                ("profile", MICROSOFT_PROFILE),
                ("productEditionId", "undefined"),
                ("SKU", sku_id),
                ("friendlyFileName", "undefined"),
                ("Locale", MICROSOFT_LOCALE),
                ("sessionID", &self.session_id),
            ],
        )?;
        let raw = self.get(
            &url,
            Some(MICROSOFT_DOWNLOAD_PAGE),
            "resolving the Windows ISO download",
        )?;
        parse_download_response(&raw)
    }

    fn get(&mut self, url: &str, referer: Option<&str>, context: &'static str) -> Result<String> {
        validate_microsoft_api_url(url)?;
        let mut request = self
            .agent
            .get(url)
            .set("User-Agent", MICROSOFT_USER_AGENT)
            .set("Accept", "application/json, text/javascript, */*; q=0.01");
        if let Some(referer) = referer {
            request = request.set("Referer", referer);
        }
        if !self.cookies.is_empty() {
            let cookies = self
                .cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            request = request.set("Cookie", &cookies);
        }
        let response = request.call().map_err(|error| http_error(context, error))?;
        for cookie in response.all("set-cookie") {
            if let Some((name, value)) = parse_set_cookie(cookie) {
                self.cookies.insert(name, value);
            }
        }
        read_response(response, context)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct MicrosoftLanguage {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Language")]
    language: String,
}

#[derive(Debug, Deserialize)]
struct MicrosoftErrorEntry {
    #[serde(rename = "Type", default)]
    error_type: f64,
    #[serde(rename = "Value", default)]
    value: String,
}

#[derive(Debug, Deserialize)]
struct LanguageResponse {
    #[serde(rename = "Skus", default)]
    skus: Vec<MicrosoftLanguage>,
    #[serde(rename = "Errors", default)]
    errors: Vec<MicrosoftErrorEntry>,
}

#[derive(Debug, Deserialize)]
struct MicrosoftDownloadOption {
    #[serde(rename = "Uri")]
    uri: String,
    #[serde(rename = "Architecture", default)]
    architecture: Option<serde_json::Value>,
    #[serde(rename = "DownloadType", default)]
    download_type: Option<u8>,
}

impl MicrosoftDownloadOption {
    fn architecture(&self) -> String {
        self.architecture
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                self.download_type.map(|download_type| match download_type {
                    0 => "x86".to_owned(),
                    1 => "x64".to_owned(),
                    2 => "ARM64".to_owned(),
                    other => format!("type_{other}"),
                })
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
struct MicrosoftDownloadResponse {
    #[serde(rename = "ProductDownloadOptions", default)]
    options: Vec<MicrosoftDownloadOption>,
    #[serde(rename = "Errors", default)]
    errors: Vec<MicrosoftErrorEntry>,
    #[serde(rename = "DownloadExpirationDatetime", default)]
    expiration: Option<String>,
}

fn parse_languages(raw: &str) -> Result<Vec<MicrosoftLanguage>> {
    let raw = unquote_json(raw)?;
    let response: LanguageResponse =
        serde_json::from_str(&raw).map_err(|_| LswError::InvalidValue {
            field: "Microsoft SKU response",
            reason: "Microsoft returned invalid JSON".to_owned(),
        })?;
    check_microsoft_errors(&response.errors)?;
    if response.skus.is_empty() {
        return Err(LswError::InvalidValue {
            field: "Microsoft SKU response",
            reason: "Microsoft returned no Windows ISO languages".to_owned(),
        });
    }
    Ok(response.skus)
}

fn parse_download_response(raw: &str) -> Result<MicrosoftDownloadResponse> {
    let raw = unquote_json(raw)?;
    let response: MicrosoftDownloadResponse =
        serde_json::from_str(&raw).map_err(|_| LswError::InvalidValue {
            field: "Microsoft download response",
            reason: "Microsoft returned invalid JSON".to_owned(),
        })?;
    check_microsoft_errors(&response.errors)?;
    if response.options.is_empty() {
        return Err(LswError::InvalidValue {
            field: "Microsoft download response",
            reason: "Microsoft returned no Windows ISO download links".to_owned(),
        });
    }
    Ok(response)
}

fn unquote_json(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('"') {
        serde_json::from_str(trimmed).map_err(|_| LswError::InvalidValue {
            field: "Microsoft response",
            reason: "Microsoft returned an invalid quoted JSON payload".to_owned(),
        })
    } else {
        Ok(trimmed.to_owned())
    }
}

fn check_microsoft_errors(errors: &[MicrosoftErrorEntry]) -> Result<()> {
    let Some(error) = errors.first() else {
        return Ok(());
    };
    let remote_message = error.value.to_ascii_lowercase();
    let reason = if error.error_type as u32 == 9
        || remote_message.contains("715-123130")
        || remote_message.contains("sentinel")
    {
        "Microsoft temporarily rejected the ISO session (715-123130); retry later or use --iso"
            .to_owned()
    } else if error.value.is_empty() {
        "Microsoft returned an ISO API error".to_owned()
    } else {
        sanitize_remote_message(&error.value)
    };
    Err(LswError::InvalidValue {
        field: "Microsoft ISO session",
        reason,
    })
}

#[derive(Debug)]
struct DownloadPageCatalog {
    product_id: String,
    hashes: BTreeMap<String, String>,
}

impl DownloadPageCatalog {
    fn hash_for(&self, language: &str, architecture: &str) -> Result<String> {
        let suffix = if architecture.eq_ignore_ascii_case("x64") {
            "64-bit"
        } else {
            architecture
        };
        let key = normalized_label(&format!("{language} {suffix}"));
        self.hashes
            .get(&key)
            .cloned()
            .ok_or_else(|| LswError::InvalidValue {
                field: "Microsoft ISO SHA-256",
                reason: format!(
                    "Microsoft's download page has no SHA-256 entry for {language} {architecture}"
                ),
            })
    }
}

fn parse_download_page(html: &str) -> Result<DownloadPageCatalog> {
    let select = html
        .find("id=\"product-edition\"")
        .and_then(|start| html.get(start..))
        .and_then(|rest| rest.split_once("</select>").map(|(select, _)| select))
        .ok_or_else(|| LswError::InvalidValue {
            field: "Microsoft download page",
            reason: "could not find the current Windows 11 product selector".to_owned(),
        })?;
    let product_id = first_numeric_option_value(select)?;

    let mut hashes = BTreeMap::new();
    let mut remaining = html;
    while let Some(row_start) = remaining.find("<tr") {
        remaining = &remaining[row_start..];
        let Some(row_end) = remaining.find("</tr>") else {
            break;
        };
        let row = &remaining[..row_end];
        remaining = &remaining[row_end + "</tr>".len()..];
        let cells = html_cells(row);
        if cells.len() != 2 {
            continue;
        }
        let hash = cells[1].trim().to_ascii_uppercase();
        if is_sha256(&hash) {
            hashes.insert(normalized_label(&cells[0]), hash);
        }
    }
    if hashes.is_empty() {
        return Err(LswError::InvalidValue {
            field: "Microsoft download page",
            reason: "could not find Microsoft's Windows ISO SHA-256 table".to_owned(),
        });
    }
    Ok(DownloadPageCatalog { product_id, hashes })
}

fn html_cells(row: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut remaining = row;
    while let Some(start) = remaining.find("<td") {
        remaining = &remaining[start..];
        let Some(open_end) = remaining.find('>') else {
            break;
        };
        remaining = &remaining[open_end + 1..];
        let Some(end) = remaining.find("</td>") else {
            break;
        };
        cells.push(strip_html(&remaining[..end]));
        remaining = &remaining[end + "</td>".len()..];
    }
    cells
}

fn strip_html(value: &str) -> String {
    let mut output = String::new();
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}

fn first_numeric_option_value(select: &str) -> Result<String> {
    let mut remaining = select;
    while let Some(option_start) = remaining.find("<option") {
        let option = &remaining[option_start..];
        let Some(tag_end) = option.find('>') else {
            break;
        };
        let tag = &option[..tag_end];
        for quote in ['"', '\''] {
            let needle = format!("value={quote}");
            if let Some(value) = tag
                .split_once(&needle)
                .map(|(_, value)| value)
                .and_then(|value| value.split_once(quote).map(|(value, _)| value.trim()))
            {
                if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Ok(value.to_owned());
                }
            }
        }
        remaining = &option[tag_end + 1..];
    }
    Err(LswError::InvalidValue {
        field: "Microsoft product ID",
        reason: "the current Windows 11 product ID is invalid".to_owned(),
    })
}

fn select_language(languages: &[MicrosoftLanguage], requested: &str) -> Result<MicrosoftLanguage> {
    let requested = normalized_label(requested);
    let matches = languages
        .iter()
        .filter(|language| normalized_label(&language.language) == requested)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [language] => Ok((*language).clone()),
        [] => Err(LswError::InvalidValue {
            field: "Windows ISO language",
            reason: format!(
                "language is unavailable; Microsoft offered {}",
                languages
                    .iter()
                    .map(|language| language.language.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
        _ => Err(LswError::InvalidValue {
            field: "Windows ISO language",
            reason: "language selection is ambiguous".to_owned(),
        }),
    }
}

fn validate_language(value: &str) -> Result<()> {
    if !value.is_empty()
        && value.len() <= 80
        && !value.contains(['\r', '\n'])
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '(' | ')' | ',')
        })
    {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "Windows ISO language",
            reason: "contains unsupported characters".to_owned(),
        })
    }
}

fn normalized_label(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect()
}

fn new_session_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| LswError::InvalidValue {
        field: "Microsoft ISO session",
        reason: "could not obtain operating-system randomness".to_owned(),
    })?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{}-{}-{}-{}-{}",
        hex_bytes(&bytes[0..4]),
        hex_bytes(&bytes[4..6]),
        hex_bytes(&bytes[6..8]),
        hex_bytes(&bytes[8..10]),
        hex_bytes(&bytes[10..16])
    ))
}

fn query_url(base: &str, pairs: &[(&str, &str)]) -> Result<String> {
    validate_microsoft_api_url(base)?;
    let mut url = Url::parse(base).map_err(|_| LswError::InvalidValue {
        field: "Microsoft API endpoint",
        reason: "internal endpoint is invalid".to_owned(),
    })?;
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    Ok(url.to_string())
}

fn validate_microsoft_api_url(value: &str) -> Result<()> {
    let url = Url::parse(value).map_err(|_| LswError::InvalidValue {
        field: "Microsoft API endpoint",
        reason: "endpoint is invalid".to_owned(),
    })?;
    let allowed = matches!(
        url.host_str().map(str::to_ascii_lowercase).as_deref(),
        Some("www.microsoft.com") | Some("vlscppe.microsoft.com") | Some("ov-df.microsoft.com")
    );
    if url.scheme() == "https" && allowed && url.username().is_empty() && url.password().is_none() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "Microsoft API endpoint",
            reason: "endpoint is outside the Microsoft HTTPS allowlist".to_owned(),
        })
    }
}

fn validate_microsoft_cdn_url(url: &Url) -> Result<()> {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let allowed = [
        ".download.prss.microsoft.com",
        ".dl.delivery.mp.microsoft.com",
        ".delivery.mp.microsoft.com",
    ]
    .iter()
    .any(|suffix| host.ends_with(suffix));
    if url.scheme() == "https"
        && allowed
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
    {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "Microsoft download URL",
            reason: "URL is outside the Microsoft HTTPS CDN allowlist".to_owned(),
        })
    }
}

fn iso_filename(url: &Url) -> Result<String> {
    let filename = url
        .path_segments()
        .and_then(Iterator::last)
        .unwrap_or_default();
    if filename.len() <= 200
        && filename.to_ascii_lowercase().ends_with(".iso")
        && filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        Ok(filename.to_owned())
    } else {
        Err(LswError::InvalidValue {
            field: "Microsoft ISO filename",
            reason: "Microsoft returned an unsafe ISO filename".to_owned(),
        })
    }
}

fn parse_set_cookie(value: &str) -> Option<(String, String)> {
    let pair = value.split(';').next()?;
    let (name, value) = pair.split_once('=')?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || value.contains(['\r', '\n', ';'])
    {
        return None;
    }
    Some((name.to_owned(), value.to_owned()))
}

fn read_response(response: ureq::Response, field: &'static str) -> Result<String> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_MICROSOFT_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MICROSOFT_RESPONSE_BYTES {
        return Err(LswError::InvalidValue {
            field,
            reason: "response exceeds 8 MiB".to_owned(),
        });
    }
    String::from_utf8(bytes).map_err(|_| LswError::InvalidValue {
        field,
        reason: "response is not valid UTF-8".to_owned(),
    })
}

fn http_error(context: &'static str, error: ureq::Error) -> LswError {
    let reason = match error {
        ureq::Error::Status(status, _) => format!("{context}: Microsoft returned HTTP {status}"),
        ureq::Error::Transport(transport) => {
            format!("{context}: network transport {:?}", transport.kind())
        }
    };
    LswError::InvalidValue {
        field: "Microsoft ISO request",
        reason,
    }
}

fn extract_mdt_w(script: &str) -> Option<String> {
    ["&w=", "?w="]
        .iter()
        .find_map(|needle| script.find(needle).map(|index| (needle, index)))
        .and_then(|(needle, index)| {
            script[index + needle.len()..]
                .split(|character: char| {
                    matches!(character, '&' | '"' | '\'' | '<' | '>' | ' ' | '\r' | '\n')
                })
                .next()
                .map(str::to_owned)
        })
        .filter(|value| !value.is_empty() && value.len() <= 512)
}

fn extract_rticks(script: &str) -> Option<String> {
    let start = script.find("rticks")?;
    let digits = script[start + "rticks".len()..]
        .trim_start_matches(|character: char| !character.is_ascii_digit())
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (digits.len() >= 10 && digits.len() <= 32).then_some(digits)
}

fn unix_time_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
        .to_string()
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sanitize_remote_message(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect()
}

mod downloader;

pub use downloader::{
    sha256_file, IsoDownloadEngine, IsoDownloadProgress, IsoDownloadProgressStage,
    IsoDownloadReport, IsoDownloader,
};

#[cfg(test)]
mod tests;

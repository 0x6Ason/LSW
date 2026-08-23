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
    /// Selected media architecture; beta.7 requires `x64`.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Download implementation selected for an ISO transfer.
pub enum IsoDownloadEngine {
    /// External aria2c with at most four CDN connections.
    Aria2,
    /// Built-in four-range resumable HTTP downloader.
    Native,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A measurable or indeterminate stage in the ISO download pipeline.
pub enum IsoDownloadProgressStage {
    /// Bytes are being transferred from Microsoft's CDN.
    Transferring,
    /// Native range files are being assembled into one ISO.
    Assembling,
    /// Downloaded or cached bytes are being hashed locally.
    Verifying,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Live progress from an official ISO download or cache verification.
pub struct IsoDownloadProgress {
    /// Operation currently being performed.
    pub stage: IsoDownloadProgressStage,
    /// Bytes completed when the selected engine can measure them faithfully.
    pub completed_bytes: Option<u64>,
    /// Total bytes expected when known.
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Verified result of an ISO download.
pub struct IsoDownloadReport {
    /// Final caller-visible ISO path.
    pub destination: PathBuf,
    /// Engine that transferred or verified the ISO.
    pub engine: IsoDownloadEngine,
    /// Final ISO size in bytes.
    pub bytes: u64,
    /// Lowercase SHA-256 calculated locally.
    pub sha256: String,
    /// Number of Microsoft URL resolutions used, including refreshes.
    pub resolutions: usize,
}

#[derive(Clone, Debug)]
/// Selects aria2c when available and otherwise performs native ranged downloads.
pub struct IsoDownloader {
    aria2c: Option<PathBuf>,
    agent: ureq::Agent,
}

impl IsoDownloader {
    /// Creates a downloader from detected host capabilities.
    pub fn new(capabilities: &HostCapabilities) -> Self {
        Self {
            aria2c: capabilities.aria2c.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(20))
                .timeout_read(Duration::from_secs(60))
                .timeout_write(Duration::from_secs(60))
                .redirects(0)
                .build(),
        }
    }

    /// Reports the engine that the next transfer will use.
    pub fn engine(&self) -> IsoDownloadEngine {
        if self.aria2c.is_some() {
            IsoDownloadEngine::Aria2
        } else {
            IsoDownloadEngine::Native
        }
    }

    /// Resolves and downloads an ISO, verifying it before atomic promotion.
    pub fn download(
        &self,
        resolver: &MicrosoftIsoResolver,
        request: &MicrosoftIsoRequest,
        destination: &Path,
    ) -> Result<IsoDownloadReport> {
        let initial = resolver.resolve(request)?;
        self.download_resolved(resolver, request, initial, destination)
    }

    /// Downloads an already-resolved ISO and refreshes expired URLs as needed.
    ///
    /// A refresh must identify the same product, SKU, filename, and digest;
    /// otherwise partial bytes are never combined.
    pub fn download_resolved(
        &self,
        resolver: &MicrosoftIsoResolver,
        request: &MicrosoftIsoRequest,
        initial: ResolvedWindowsIso,
        destination: &Path,
    ) -> Result<IsoDownloadReport> {
        self.download_resolved_with_progress(resolver, request, initial, destination, |_| {})
    }

    /// Downloads an already-resolved ISO while reporting transfer and hash progress.
    pub fn download_resolved_with_progress<F>(
        &self,
        resolver: &MicrosoftIsoResolver,
        request: &MicrosoftIsoRequest,
        initial: ResolvedWindowsIso,
        destination: &Path,
        mut on_progress: F,
    ) -> Result<IsoDownloadReport>
    where
        F: FnMut(&IsoDownloadProgress),
    {
        let parent = destination.parent().ok_or_else(|| LswError::InvalidValue {
            field: "ISO destination",
            reason: "destination has no parent directory".to_owned(),
        })?;
        require_real_directory(parent, "ISO destination directory")?;
        validate_destination(destination)?;

        if let Some(report) =
            existing_verified_iso(destination, &initial, self.engine(), &mut on_progress)?
        {
            return Ok(report);
        }
        let temporary = download_temporary_path(destination)?;
        let engine = self.engine();
        let mut resolved = initial.clone();
        let mut last_error = None;

        for attempt in 0..MAX_DOWNLOAD_ATTEMPTS {
            if attempt > 0 {
                resolved = resolver.resolve(request)?;
                ensure_same_iso(&initial, &resolved)?;
            }
            let result = match &self.aria2c {
                Some(aria2c) => {
                    download_with_aria2(aria2c, &resolved, &temporary, &mut on_progress)
                }
                None => download_with_ranges(&self.agent, &resolved, &temporary, &mut on_progress),
            };
            match result {
                Ok(()) => {
                    let actual = sha256_file_with_progress(&temporary, &mut on_progress)?;
                    if !actual.eq_ignore_ascii_case(&resolved.expected_sha256) {
                        return Err(LswError::InvalidValue {
                            field: "Windows ISO SHA-256",
                            reason: format!(
                                "downloaded ISO hash {actual} does not match Microsoft's expected {}",
                                resolved.expected_sha256
                            ),
                        });
                    }
                    let bytes = fs::metadata(&temporary)?.len();
                    promote_download(&temporary, destination)?;
                    cleanup_download_sidecars(&temporary);
                    return Ok(IsoDownloadReport {
                        destination: destination.to_owned(),
                        engine,
                        bytes,
                        sha256: actual,
                        resolutions: attempt + 1,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "download failed without an error".to_owned(),
        }))
    }
}

fn download_with_aria2<F>(
    aria2c: &Path,
    resolved: &ResolvedWindowsIso,
    temporary: &Path,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(&IsoDownloadProgress),
{
    validate_microsoft_cdn_url(&resolved.download_url.0)?;
    let parent = temporary.parent().ok_or_else(|| LswError::InvalidValue {
        field: "ISO download",
        reason: "temporary destination has no parent".to_owned(),
    })?;
    let filename = temporary
        .file_name()
        .ok_or_else(|| LswError::InvalidValue {
            field: "ISO download",
            reason: "temporary destination has no filename".to_owned(),
        })?;
    let filename = filename.to_str().ok_or_else(|| LswError::InvalidValue {
        field: "ISO download",
        reason: "aria2 requires a UTF-8 destination filename".to_owned(),
    })?;
    if filename.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err(LswError::InvalidValue {
            field: "ISO download",
            reason: "aria2 destination filename contains a line break".to_owned(),
        });
    }
    validate_partial_file(temporary)?;

    let mut child = Command::new(aria2c)
        .arg("--input-file=-")
        .arg("--continue=true")
        .arg("--max-connection-per-server=4")
        .arg("--split=4")
        .arg("--min-split-size=16M")
        .arg("--max-tries=2")
        .arg("--retry-wait=2")
        .arg("--connect-timeout=20")
        .arg("--timeout=60")
        .arg("--file-allocation=none")
        .arg("--allow-overwrite=false")
        .arg("--auto-file-renaming=false")
        .arg("--console-log-level=warn")
        .arg("--summary-interval=0")
        .arg("--quiet=true")
        .current_dir(parent)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // Global --out is ignored when aria2 reads URIs from an input file.
        // Keep the signed URL off argv and provide the output name as a
        // per-URI option on the following indented line.
        stdin.write_all(resolved.download_url.expose().as_bytes())?;
        stdin.write_all(format!("\n  out={filename}\n").as_bytes())?;
    }
    on_progress(&IsoDownloadProgress {
        stage: IsoDownloadProgressStage::Transferring,
        completed_bytes: None,
        total_bytes: None,
    });
    let mut last_heartbeat = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(1) {
            on_progress(&IsoDownloadProgress {
                stage: IsoDownloadProgressStage::Transferring,
                completed_bytes: None,
                total_bytes: None,
            });
            last_heartbeat = Instant::now();
        }
        thread::sleep(Duration::from_millis(200));
    };
    if !status.success() {
        return Err(LswError::ExternalCommandFailed {
            program: aria2c.to_owned(),
            status: status.code(),
        });
    }
    require_regular_file(temporary, "aria2 ISO download")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

fn split_ranges(length: u64) -> Result<Vec<ByteRange>> {
    if length == 0 {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "Microsoft CDN reported an empty file".to_owned(),
        });
    }
    let connections = usize::try_from(length.min(MAX_CDN_CONNECTIONS as u64)).unwrap_or(1);
    let chunk = length.div_ceil(connections as u64);
    Ok((0..connections)
        .map(|index| {
            let start = index as u64 * chunk;
            ByteRange {
                start,
                end: (start + chunk - 1).min(length - 1),
            }
        })
        .filter(|range| range.start <= range.end)
        .collect())
}

fn download_with_ranges<F>(
    agent: &ureq::Agent,
    resolved: &ResolvedWindowsIso,
    temporary: &Path,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(&IsoDownloadProgress),
{
    validate_microsoft_cdn_url(&resolved.download_url.0)?;
    let length = cdn_content_length(agent, &resolved.download_url)?;
    let ranges = split_ranges(length)?;
    let part_paths = ranges
        .iter()
        .enumerate()
        .map(|(index, _)| sidecar_path(temporary, &format!("part{index}")))
        .collect::<Result<Vec<_>>>()?;

    let mut downloaded = part_paths
        .iter()
        .zip(&ranges)
        .map(|(path, range)| partial_file_length(path, range.len()))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum::<u64>();
    on_progress(&IsoDownloadProgress {
        stage: IsoDownloadProgressStage::Transferring,
        completed_bytes: Some(downloaded),
        total_bytes: Some(length),
    });

    thread::scope(|scope| -> Result<()> {
        let (progress_sender, progress_receiver) = mpsc::channel();
        let mut handles = Vec::new();
        for (range, path) in ranges.iter().copied().zip(part_paths.iter().cloned()) {
            let agent = agent.clone();
            let url = resolved.download_url.clone();
            let progress_sender = progress_sender.clone();
            handles.push(
                scope.spawn(move || download_range(&agent, &url, range, &path, &progress_sender)),
            );
        }
        drop(progress_sender);
        for bytes in progress_receiver {
            downloaded = downloaded.saturating_add(bytes).min(length);
            on_progress(&IsoDownloadProgress {
                stage: IsoDownloadProgressStage::Transferring,
                completed_bytes: Some(downloaded),
                total_bytes: Some(length),
            });
        }
        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Ok(Err(_)) => {}
                Err(_) if first_error.is_none() => {
                    first_error = Some(LswError::InvalidValue {
                        field: "Windows ISO download",
                        reason: "a range worker panicked".to_owned(),
                    })
                }
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    })?;

    let assembly = sidecar_path(temporary, "assembling")?;
    let mut output = create_private_truncated_file(&assembly)?;
    let mut assembled = 0_u64;
    on_progress(&IsoDownloadProgress {
        stage: IsoDownloadProgressStage::Assembling,
        completed_bytes: Some(0),
        total_bytes: Some(length),
    });
    let mut buffer = vec![0_u8; 1024 * 1024];
    for (path, range) in part_paths.iter().zip(&ranges) {
        require_regular_file(path, "ISO range part")?;
        let metadata = fs::metadata(path)?;
        if metadata.len() != range.len() {
            return Err(LswError::InvalidValue {
                field: "Windows ISO download",
                reason: "a completed range has the wrong size".to_owned(),
            });
        }
        let mut part = fs::File::open(path)?;
        loop {
            let bytes = part.read(&mut buffer)?;
            if bytes == 0 {
                break;
            }
            output.write_all(&buffer[..bytes])?;
            assembled += bytes as u64;
            on_progress(&IsoDownloadProgress {
                stage: IsoDownloadProgressStage::Assembling,
                completed_bytes: Some(assembled.min(length)),
                total_bytes: Some(length),
            });
        }
    }
    if assembled != length {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "assembled ISO has the wrong size".to_owned(),
        });
    }
    output.flush()?;
    output.sync_all()?;
    drop(output);
    if fs::symlink_metadata(temporary).is_ok() {
        validate_partial_file(temporary)?;
        fs::remove_file(temporary)?;
    }
    fs::rename(assembly, temporary)?;
    set_private_file_permissions(temporary)
}

fn cdn_content_length(agent: &ureq::Agent, url: &SecretDownloadUrl) -> Result<u64> {
    let response = agent
        .head(url.expose())
        .set("User-Agent", MICROSOFT_USER_AGENT)
        .call()
        .map_err(|error| download_http_error("checking ISO size", error))?;
    validate_final_cdn_response(&response)?;
    response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|length| *length > 0)
        .ok_or_else(|| LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "Microsoft CDN returned no valid Content-Length".to_owned(),
        })
}

fn download_range(
    agent: &ureq::Agent,
    url: &SecretDownloadUrl,
    range: ByteRange,
    path: &Path,
    progress_sender: &mpsc::Sender<u64>,
) -> Result<()> {
    let existing = partial_file_length(path, range.len())?;
    if existing == range.len() {
        return Ok(());
    }
    let start = range.start + existing;
    let response = agent
        .get(url.expose())
        .set("User-Agent", MICROSOFT_USER_AGENT)
        .set("Accept-Encoding", "identity")
        .set("Range", &format!("bytes={start}-{}", range.end))
        .call()
        .map_err(|error| download_http_error("downloading an ISO range", error))?;
    validate_final_cdn_response(&response)?;
    if response.status() != 206 {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: format!(
                "Microsoft CDN did not honor a Range request (HTTP {})",
                response.status()
            ),
        });
    }
    let expected_content_range = format!("bytes {start}-{}/", range.end);
    if !response
        .header("Content-Range")
        .is_some_and(|value| value.starts_with(&expected_content_range))
    {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "Microsoft CDN returned an unexpected Content-Range".to_owned(),
        });
    }
    let remaining = range.len() - existing;
    let mut output = OpenOptions::new().create(true).append(true).open(path)?;
    set_private_file_permissions(path)?;
    let mut reader = response.into_reader().take(remaining + 1);
    let mut written = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let bytes = reader.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        output.write_all(&buffer[..bytes])?;
        written += bytes as u64;
        let _ = progress_sender.send(bytes as u64);
    }
    output.flush()?;
    output.sync_all()?;
    if written != remaining {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: format!(
                "Microsoft CDN range ended early (expected {remaining} bytes, received {written})"
            ),
        });
    }
    Ok(())
}

fn validate_final_cdn_response(response: &ureq::Response) -> Result<()> {
    let url = Url::parse(response.get_url()).map_err(|_| LswError::InvalidValue {
        field: "Windows ISO download",
        reason: "Microsoft CDN returned an invalid final URL".to_owned(),
    })?;
    validate_microsoft_cdn_url(&url)
}

fn download_http_error(context: &'static str, error: ureq::Error) -> LswError {
    let reason = match error {
        ureq::Error::Status(status, _) => format!("{context}: CDN returned HTTP {status}"),
        ureq::Error::Transport(transport) => {
            format!("{context}: network transport {:?}", transport.kind())
        }
    };
    LswError::InvalidValue {
        field: "Windows ISO download",
        reason,
    }
}

fn existing_verified_iso<F>(
    destination: &Path,
    resolved: &ResolvedWindowsIso,
    engine: IsoDownloadEngine,
    on_progress: &mut F,
) -> Result<Option<IsoDownloadReport>>
where
    F: FnMut(&IsoDownloadProgress),
{
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!("{} is not a regular file", destination.display()),
        });
    }
    let actual = sha256_file_with_progress(destination, on_progress)?;
    if !actual.eq_ignore_ascii_case(&resolved.expected_sha256) {
        return Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!(
                "{} already exists but its SHA-256 does not match Microsoft's expected hash",
                destination.display()
            ),
        });
    }
    Ok(Some(IsoDownloadReport {
        destination: destination.to_owned(),
        engine,
        bytes: metadata.len(),
        sha256: actual,
        resolutions: 1,
    }))
}

fn ensure_same_iso(initial: &ResolvedWindowsIso, refreshed: &ResolvedWindowsIso) -> Result<()> {
    if initial.product_id == refreshed.product_id
        && initial.sku_id == refreshed.sku_id
        && initial.filename == refreshed.filename
        && initial
            .expected_sha256
            .eq_ignore_ascii_case(&refreshed.expected_sha256)
    {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "Microsoft ISO refresh",
            reason: "Microsoft changed the selected ISO while refreshing an expired URL; refusing to combine downloads"
                .to_owned(),
        })
    }
}

/// Calculates a lowercase SHA-256 for a regular, non-symlink file.
pub fn sha256_file(path: &Path) -> Result<String> {
    sha256_file_with_progress(path, &mut |_| {})
}

fn sha256_file_with_progress<F>(path: &Path, on_progress: &mut F) -> Result<String>
where
    F: FnMut(&IsoDownloadProgress),
{
    require_regular_file(path, "SHA-256 input")?;
    let total = fs::metadata(path)?.len();
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut completed = 0_u64;
    on_progress(&IsoDownloadProgress {
        stage: IsoDownloadProgressStage::Verifying,
        completed_bytes: Some(0),
        total_bytes: Some(total),
    });
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        digest.update(&buffer[..bytes]);
        completed += bytes as u64;
        on_progress(&IsoDownloadProgress {
            stage: IsoDownloadProgressStage::Verifying,
            completed_bytes: Some(completed),
            total_bytes: Some(total),
        });
    }
    Ok(hex_bytes(&digest.finalize()))
}

fn validate_destination(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!("{} must not be a symbolic link", path.display()),
        }),
        Ok(metadata) if !metadata.file_type().is_file() => Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!("{} is not a regular file", path.display()),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn promote_download(temporary: &Path, destination: &Path) -> Result<()> {
    require_regular_file(temporary, "completed ISO download")?;
    set_private_file_permissions(temporary)?;
    match fs::hard_link(temporary, destination) {
        Ok(()) => {
            fs::remove_file(temporary)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!(
                "{} appeared while the ISO was downloading; refusing to overwrite it",
                destination.display()
            ),
        }),
        Err(error) => Err(error.into()),
    }
}

fn validate_partial_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(LswError::InvalidValue {
            field: "ISO partial download",
            reason: format!("{} is not a regular file", path.display()),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn partial_file_length(path: &Path, expected: u64) -> Result<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= expected =>
        {
            Ok(metadata.len())
        }
        Ok(metadata) if metadata.file_type().is_symlink() => Err(LswError::InvalidValue {
            field: "ISO range part",
            reason: format!("{} must not be a symbolic link", path.display()),
        }),
        Ok(_) => Err(LswError::InvalidValue {
            field: "ISO range part",
            reason: format!("{} has an invalid type or size", path.display()),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn download_temporary_path(destination: &Path) -> Result<PathBuf> {
    let filename = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| LswError::InvalidValue {
            field: "ISO destination",
            reason: "filename must be valid UTF-8".to_owned(),
        })?;
    Ok(destination.with_file_name(format!(".{filename}.lsw-download")))
}

fn sidecar_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| LswError::InvalidValue {
            field: "ISO partial download",
            reason: "filename must be valid UTF-8".to_owned(),
        })?;
    Ok(path.with_file_name(format!("{filename}.{suffix}")))
}

fn cleanup_download_sidecars(temporary: &Path) {
    for index in 0..MAX_CDN_CONNECTIONS {
        if let Ok(path) = sidecar_path(temporary, &format!("part{index}")) {
            let _ = remove_private_regular_file(&path);
        }
    }
    for suffix in ["assembling", "aria2"] {
        if let Ok(path) = sidecar_path(temporary, suffix) {
            let _ = remove_private_regular_file(&path);
        }
    }
}

fn remove_private_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(LswError::InvalidValue {
            field: "ISO partial download",
            reason: format!("refusing to remove non-regular {}", path.display()),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn create_private_truncated_file(path: &Path) -> Result<fs::File> {
    validate_partial_file(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    set_private_file_permissions(path)?;
    Ok(file)
}

fn require_real_directory(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a real directory", path.display()),
        })
    }
}

fn require_regular_file(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a regular file", path.display()),
        })
    }
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "lsw-iso-download-test-{}-{nonce}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("fixture should be created");
        root
    }

    #[test]
    fn parses_current_product_and_official_sha_table() {
        let html = r#"
            <select id="product-edition">
              <option value="">Select edition</option>
              <option value="3321">Windows 11 multi-edition</option>
            </select>
            <table><tbody>
              <tr><th>Country Locale</th><th>Hash Code</th></tr>
              <tr><td>English 64-bit</td><td>768984706B909479417B2368438909440F2967FF05C6A9195ED2667254E465E3</td></tr>
              <tr><td>French 64-bit</td><td>A02693BEB8EB166AFDFDB7DB49176A2B547F81E61030A695FE172277DB6A1977</td></tr>
            </tbody></table>
        "#;
        let catalog = parse_download_page(html).expect("download page should parse");
        assert_eq!(catalog.product_id, "3321");
        assert_eq!(
            catalog
                .hash_for("English", "x64")
                .expect("English hash should exist"),
            "768984706B909479417B2368438909440F2967FF05C6A9195ED2667254E465E3"
        );
        assert!(catalog.hash_for("German", "x64").is_err());
    }

    #[test]
    fn parses_microsoft_language_and_download_payloads() {
        let languages = parse_languages(
            r#"{"Skus":[{"Id":"123","Language":"English"},{"Id":"456","Language":"French"}]}"#,
        )
        .expect("language response should parse");
        assert_eq!(
            select_language(&languages, "english")
                .expect("language should match")
                .id,
            "123"
        );

        let response = parse_download_response(
            r#"{"ProductDownloadOptions":[{"Uri":"https://software.download.prss.microsoft.com/path/windows.iso?t=secret","DownloadType":1}],"DownloadExpirationDatetime":"2026-08-18T00:00:00Z"}"#,
        )
        .expect("download response should parse");
        assert_eq!(response.options[0].architecture(), "x64");
        assert_eq!(response.expiration.as_deref(), Some("2026-08-18T00:00:00Z"));
    }

    #[test]
    fn signed_urls_are_allowlisted_and_redacted() {
        let url = SecretDownloadUrl::parse(
            "https://software.download.prss.microsoft.com/path/windows.iso?t=secret&P1=token",
        )
        .expect("Microsoft CDN URL should be accepted");
        let debug = format!("{url:?}");
        assert!(debug.contains("software.download.prss.microsoft.com/path/windows.iso"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("token"));
        assert!(SecretDownloadUrl::parse(
            "http://software.download.prss.microsoft.com/path/windows.iso?t=secret"
        )
        .is_err());
        assert!(SecretDownloadUrl::parse(
            "https://software.download.prss.microsoft.com.evil.example/windows.iso?t=secret"
        )
        .is_err());
    }

    #[test]
    fn native_ranges_use_no_more_than_four_connections_and_cover_every_byte() {
        assert_eq!(
            split_ranges(10).expect("ranges should split"),
            vec![
                ByteRange { start: 0, end: 2 },
                ByteRange { start: 3, end: 5 },
                ByteRange { start: 6, end: 8 },
                ByteRange { start: 9, end: 9 },
            ]
        );
        assert_eq!(split_ranges(3).expect("ranges should split").len(), 3);
        assert!(split_ranges(0).is_err());
    }

    #[test]
    fn sha256_verification_is_exact_and_existing_mismatches_fail_closed() {
        let root = fixture();
        let iso = root.join("windows.iso");
        fs::write(&iso, b"abc").expect("fixture should be written");
        assert_eq!(
            sha256_file(&iso).expect("hash should compute"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let resolved = ResolvedWindowsIso {
            product_id: "3321".to_owned(),
            sku_id: "123".to_owned(),
            language: "English".to_owned(),
            architecture: "x64".to_owned(),
            filename: "windows.iso".to_owned(),
            expected_sha256: "0".repeat(64),
            expires_at: None,
            download_url: SecretDownloadUrl::parse(
                "https://software.download.prss.microsoft.com/path/windows.iso?t=secret",
            )
            .expect("URL should parse"),
        };
        assert!(
            existing_verified_iso(&iso, &resolved, IsoDownloadEngine::Native, &mut |_| {}).is_err()
        );
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn completed_download_never_overwrites_a_destination_that_appeared() {
        let root = fixture();
        let temporary = root.join(".windows.iso.lsw-download");
        let destination = root.join("windows.iso");
        fs::write(&temporary, b"verified download").expect("temporary should be written");
        fs::write(&destination, b"unrelated file").expect("destination should be written");

        let error = promote_download(&temporary, &destination)
            .expect_err("existing destination must not be overwritten");
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            fs::read(&destination).expect("destination should remain readable"),
            b"unrelated file"
        );
        assert!(temporary.exists());
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn sha256_reports_exact_bounded_progress() {
        let root = fixture();
        let input = root.join("input.iso");
        fs::write(&input, vec![0x5a; 1024 * 1024 + 17]).expect("fixture should be written");
        let mut events = Vec::new();
        let digest = sha256_file_with_progress(&input, &mut |event| events.push(*event))
            .expect("fixture should hash");

        assert_eq!(digest.len(), 64);
        assert_eq!(
            events.first(),
            Some(&IsoDownloadProgress {
                stage: IsoDownloadProgressStage::Verifying,
                completed_bytes: Some(0),
                total_bytes: Some(1024 * 1024 + 17),
            })
        );
        assert_eq!(
            events.last().and_then(|event| event.completed_bytes),
            Some(1024 * 1024 + 17)
        );
        assert!(events.iter().all(|event| {
            event.completed_bytes.unwrap_or_default() <= event.total_bytes.unwrap_or_default()
        }));
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn parses_mdt_fingerprint_values_and_generates_uuid_v4_sessions() {
        let script = "x?foo=1&w=abc%2B123&bar=2; rticks=+1787061234567";
        assert_eq!(extract_mdt_w(script).as_deref(), Some("abc%2B123"));
        assert_eq!(extract_rticks(script).as_deref(), Some("1787061234567"));
        let session = new_session_id().expect("session ID should be generated");
        assert_eq!(session.len(), 36);
        assert_eq!(session.as_bytes()[14], b'4');
        assert!(matches!(session.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn microsoft_rate_limit_errors_do_not_echo_remote_payloads_unbounded() {
        let error = parse_languages(
            r#"{"Errors":[{"Type":9,"Value":"715-123130 https://example.invalid/?token=secret"}]}"#,
        )
        .expect_err("rate limit should fail");
        let message = error.to_string();
        assert!(message.contains("715-123130"));
        assert!(!message.contains("token=secret"));

        let sentinel = parse_languages(
            r#"{"Errors":[{"Type":0,"Value":"Sentinel marked this request as rejected."}]}"#,
        )
        .expect_err("Sentinel rejection should fail");
        assert!(sentinel.to_string().contains("retry later or use --iso"));
    }

    #[test]
    fn aria2_adapter_keeps_the_signed_url_off_argv_and_honors_the_output_name() {
        let root = fixture();
        let fake_aria2 = root.join("aria2c");
        fs::write(
            &fake_aria2,
            "#!/bin/sh\n\
             for argument in \"$@\"; do\n\
               case \"$argument\" in\n\
                 --max-redirect=*|--dir=*|--out=*) exit 91 ;;\n\
                 *token=*) exit 92 ;;\n\
               esac\n\
             done\n\
             url=\n\
             output=\n\
             while IFS= read -r line; do\n\
               case \"$line\" in\n\
                 '  out='*) output=${line#*out=} ;;\n\
                 '  '*) ;;\n\
                 *) url=$line ;;\n\
               esac\n\
             done\n\
             case \"$url\" in\n\
               https://software.download.prss.microsoft.com/*token=*) ;;\n\
               *) exit 93 ;;\n\
             esac\n\
             [ \"$output\" = '.windows.iso.lsw-download' ] || exit 94\n\
             printf downloaded > \"$output\"\n",
        )
        .expect("fake aria2 should be written");
        fs::set_permissions(&fake_aria2, fs::Permissions::from_mode(0o700))
            .expect("fake aria2 should be executable");

        let resolved = ResolvedWindowsIso {
            product_id: "product".to_owned(),
            sku_id: "sku".to_owned(),
            language: "English".to_owned(),
            architecture: "x64".to_owned(),
            filename: "windows.iso".to_owned(),
            expected_sha256: "0".repeat(64),
            expires_at: None,
            download_url: SecretDownloadUrl::parse(
                "https://software.download.prss.microsoft.com/windows.iso?token=secret",
            )
            .expect("fixture URL should be accepted"),
        };
        let temporary = root.join(".windows.iso.lsw-download");
        download_with_aria2(&fake_aria2, &resolved, &temporary, &mut |_| {})
            .expect("fake aria2 should receive a valid input-file request");
        assert_eq!(
            fs::read(&temporary).expect("download should exist"),
            b"downloaded"
        );
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    #[ignore = "requires live Microsoft endpoints"]
    fn resolves_live_microsoft_iso_without_exposing_its_token() {
        let resolved = MicrosoftIsoResolver::new()
            .resolve(&MicrosoftIsoRequest::default())
            .expect("live Microsoft resolver should succeed");
        assert!(is_sha256(&resolved.expected_sha256));
        assert_eq!(resolved.architecture, "x64");
        assert!(!format!("{resolved:?}").contains("P1="));
    }
}

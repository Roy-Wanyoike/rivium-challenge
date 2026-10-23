//! The digest job: fetch a droidcon page, extract `<title>`, hash the body.
//!
//! Named `Digested` (not `Digest`) because `sha2` exports a `Digest` trait
//! that collides in scope.

use sha2::{Digest, Sha256};
use std::time::Duration;

/// Task arguments flowing through the queue; `id` is the ledger key.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DigestJob {
    pub id: String,
    pub url: String,
}

/// Result of a successful digest.
#[derive(Clone, Debug)]
pub struct Digested {
    pub title: Option<String>,
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("http request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("http status {0}")]
    Status(reqwest::StatusCode),
    #[error("response body too large ({0} bytes)")]
    TooLarge(usize),
}

const MAX_BODY: usize = 8 * 1024 * 1024;

/// Fetch `url` and produce title + SHA-256 of the raw body bytes.
pub async fn digest(
    client: &reqwest::Client,
    url: &str,
) -> Result<Digested, JobError> {
    let response = client.get(url).send().await?.error_for_status()?;
    let status = response.status();
    if !status.is_success() {
        return Err(JobError::Status(status));
    }
    let body = response.bytes().await?;
    if body.len() > MAX_BODY {
        return Err(JobError::TooLarge(body.len()));
    }
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    // Body bytes may not be UTF-8; only the title path needs text.
    let title = extract_title(&String::from_utf8_lossy(&body));
    Ok(Digested { title, sha256 })
}

/// Build the shared HTTP client with the configured timeout and user agent.
pub fn build_client(timeout: Duration, user_agent: &str) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(user_agent)
        .gzip(true)
        .build()
}

/// Locate and decode `<title>…</title>` without pulling in a regex engine.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let open = lower.find("<title")?;
    // Skip attributes: title tag content starts after the closing '>'.
    let after_tag = html.get(open..)?;
    let gt = after_tag.find('>')?;
    let rest = &after_tag[gt + 1..];
    let close = rest.to_lowercase().find("</title")?;
    let raw = rest.get(..close)?;
    let decoded = decode_entities(raw);
    let trimmed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Decode HTML named and numeric (`&#NN;`, `&#xHH;`) entities.
/// `&#x27;`-style numeric entities appear all over real-world CMS output.
pub fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let end = tail.find(';').unwrap_or(tail.len());
        let entity = &tail[1..end.min(12)];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            other => {
                if let Some(hex) = other
                    .strip_prefix("#x")
                    .or_else(|| other.strip_prefix("#X"))
                {
                    u32::from_str_radix(hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                } else if let Some(dec) = other.strip_prefix('#') {
                    dec.parse::<u32>()
                        .ok()
                        .and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = tail[end.min(tail.len())..].trim_start_matches(';');
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_normalises_title() {
        let html = "<html><head><TITLE  id=x>droidcon   Kenya 2026</TITLE></head></html>";
        assert_eq!(extract_title(html).as_deref(), Some("droidcon Kenya 2026"));
    }

    #[test]
    fn decodes_named_numeric_and_hex_entities() {
        assert_eq!(decode_entities("droidcon&#x27;s"), "droidcon's");
        assert_eq!(decode_entities("A &#38; B"), "A & B");
        assert_eq!(decode_entities("home &amp; away &lt;3"), "home & away <3");
        // Multi-byte numeric entity must round-trip through char::from_u32.
        assert_eq!(decode_entities("&#128512;"), "\u{1f600}");
        // Unknown entity passes through untouched.
        assert_eq!(decode_entities("a &bogus; b"), "a &bogus; b");
    }

    #[test]
    fn utf8_boundary_slicing_is_safe() {
        // The 'é' at a cut boundary must not panic during whitespace folding.
        let html = "<title>café — droidcon Nairobi 🇰🇪</title>";
        let title = extract_title(html).unwrap();
        assert!(title.contains("café"));
        assert!(title.ends_with("droidcon Nairobi \u{1f1f0}\u{1f1ea}"));
    }

    #[tokio::test]
    #[ignore = "requires network"]
    async fn digests_droidcon_homepage() {
        let client = build_client(Duration::from_secs(20), "session-digger/0.1").unwrap();
        let out = digest(&client, "https://droidcon.co.ke").await.unwrap();
        assert_eq!(out.sha256.len(), 64);
        println!("title={:?} sha256={}", out.title, out.sha256);
    }
}

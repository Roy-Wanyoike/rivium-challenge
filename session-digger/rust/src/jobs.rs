//! The digest job: fetch a droidcon page, extract `<title>`, hash the body —
//! and, when the page is the droidcon speakers directory, harvest the
//! speaker list.
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
    /// Non-empty only for the droidcon speakers directory.
    pub speakers: Vec<Speaker>,
}

/// One speaker harvested from the droidcon speakers page.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Speaker {
    pub name: String,
    pub role: Option<String>,
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

/// Path suffix identifying the droidcon Kenya speakers directory — the one
/// page whose digest additionally yields the speaker list.
pub const SPEAKERS_PATH: &str = "/speakers";

/// Whether `url` points at the droidcon speakers directory (any depth, with
/// or without a trailing slash; query strings are ignored).
pub fn is_speakers_page(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(parsed) => parsed
            .path()
            .trim_end_matches('/')
            .ends_with(SPEAKERS_PATH),
        Err(_) => false,
    }
}

/// Fetch `url` and produce title + SHA-256 of the raw body bytes. For the
/// droidcon speakers directory the speaker cards are harvested as well.
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
    // Body bytes may not be UTF-8; only the text paths need a lossy view.
    let text = String::from_utf8_lossy(&body);
    let title = extract_title(&text);
    let speakers = if is_speakers_page(url) {
        extract_speakers(&text)
    } else {
        Vec::new()
    };
    Ok(Digested {
        title,
        sha256,
        speakers,
    })
}

/// Build the shared HTTP client with the configured timeout and user agent.
pub fn build_client(timeout: Duration, user_agent: &str) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(user_agent)
        .gzip(true)
        .build()
}

/// ASCII-case-insensitive first occurrence of `needle` at/after `from`.
/// Tag names are ASCII, so this is exact for HTML while staying Unicode-safe:
/// no case-folded copy whose byte offsets could drift from the original.
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = hay.get(from..)?;
    let nb = needle.as_bytes();
    let hb = hay.as_bytes();
    if nb.is_empty() || nb.len() > hb.len() {
        return None;
    }
    (0..=hb.len() - nb.len())
        .find(|&i| hb[i..i + nb.len()].eq_ignore_ascii_case(nb))
        .map(|i| from + i)
}

/// Collapse entity-decoded text into single-spaced, trimmed content.
fn fold_text(raw: &str) -> String {
    decode_entities(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Locate and decode `<title>…</title>` without pulling in a regex engine.
fn extract_title(html: &str) -> Option<String> {
    let open = find_ci(html, "<title", 0)?;
    // Skip attributes: title tag content starts after the closing '>'.
    let gt = html.get(open..)?.find('>')? + open;
    let close = find_ci(html, "</title", gt + 1)?;
    let trimmed = fold_text(html.get(gt + 1..close)?);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Upper bound on harvested speakers: the directory lists dozens, not
/// thousands; the cap keeps a hostile page from ballooning the ledger.
const MAX_SPEAKERS: usize = 200;

/// Harvest speaker cards (`<h4>Name</h4><p>Role</p>` pairs) from the droidcon
/// speakers page HTML — same no-regex approach as [`extract_title`]. Returns
/// an empty list for pages without that card markup.
pub fn extract_speakers(html: &str) -> Vec<Speaker> {
    let mut speakers = Vec::new();
    let mut cursor = 0usize;
    while speakers.len() < MAX_SPEAKERS {
        // Next `<h4` whose name is non-empty.
        let open = match find_ci(html, "<h4", cursor) {
            Some(open) => open,
            None => break,
        };
        let name_open = match open_tag_body(html, open, "h4") {
            Some((body_start, body_end)) => {
                cursor = body_end;
                body_start
            }
            // `<h4` that is really `<h40`/… — resume the scan after it.
            None => {
                cursor = open + 3;
                continue;
            }
        };
        let close = match find_ci(html, "</h4", name_open) {
            Some(close) => close,
            None => break,
        };
        let name = fold_text(html.get(name_open..close).unwrap_or(""));
        if name.is_empty() {
            continue;
        }
        // Role: the first <p> after this name but before the next <h4>.
        let role = role_after(html, close);
        speakers.push(Speaker { name, role });
        cursor = close;
    }
    speakers
}

/// Given the offset of `<h4`, find where its inner text starts and where the
/// whole element scan should resume. `None` means "not a real `<h4>` tag".
fn open_tag_body(html: &str, open: usize, tag: &str) -> Option<(usize, usize)> {
    let after = html.as_bytes().get(open + 1 + tag.len()).copied()?;
    // `<h4` must not be a prefix of a longer tag name such as `<h40`.
    if !(after == b'>' || after == b' ' || after == b'\t' || after == b'\n' || after == b'\r') {
        return None;
    }
    let gt = html.get(open..)?.find('>')? + open;
    Some((gt + 1, open + 1 + tag.len()))
}

/// The first `<p>…</p>` between `from` and the next `<h4`, decoded.
fn role_after(html: &str, from: usize) -> Option<String> {
    let next_h4 = find_ci(html, "<h4", from).unwrap_or(html.len());
    let mut cursor = from;
    loop {
        let p_open = find_ci(html, "<p", cursor)?;
        if p_open >= next_h4 {
            return None;
        }
        match open_tag_body(html, p_open, "p") {
            Some((body_start, resume)) => {
                let close = match find_ci(html, "</p", body_start) {
                    Some(close) if close < next_h4 => close,
                    _ => {
                        cursor = resume;
                        continue;
                    }
                };
                let role = fold_text(html.get(body_start..close).unwrap_or(""));
                return (!role.is_empty()).then_some(role);
            }
            // `<p` that is really `<pre`/`<param`/… — keep scanning after it.
            None => cursor = p_open + 2,
        }
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
        assert!(out.speakers.is_empty(), "homepage must not yield speakers");
        println!("title={:?} sha256={}", out.title, out.sha256);
    }

    #[tokio::test]
    #[ignore = "requires network"]
    async fn digests_droidcon_speakers_directory() {
        let client = build_client(Duration::from_secs(20), "session-digger/0.1").unwrap();
        let out = digest(&client, "https://droidcon.co.ke/speakers").await.unwrap();
        assert!(out.speakers.len() >= 5, "expected the full speaker list");
        assert!(out.speakers.iter().all(|s| !s.name.is_empty()));
        println!("{} speakers:", out.speakers.len());
        for s in &out.speakers {
            println!("  · {} — {}", s.name, s.role.as_deref().unwrap_or(""));
        }
    }

    #[test]
    fn speakers_page_gate_matches_path_not_query() {
        assert!(is_speakers_page("https://droidcon.co.ke/speakers"));
        assert!(is_speakers_page("https://droidcon.co.ke/speakers/"));
        assert!(is_speakers_page("https://droidcon.co.ke/2024/speakers"));
        assert!(!is_speakers_page("https://droidcon.co.ke/"));
        assert!(!is_speakers_page(
            "https://droidcon.co.ke/sessions/x?from=%2Fspeakers"
        ));
        assert!(!is_speakers_page("not a url"));
    }

    #[test]
    fn extracts_speaker_cards_from_real_markup() {
        // Structure lifted verbatim from droidcon.co.ke/speakers (Tailwind
        // classes included so attribute skipping stays honest).
        let html = concat!(
            "<html><head><title>Speakers — droidcon Kenya</title></head><body>",
            "<div class=\"grid grid-cols-2 md:grid-cols-4 gap-4\">",
            "<a class=\"group flex flex-col h-full\" href=\"/sessions/a?from=%2Fspeakers\">",
            "<img class=\"w-full aspect-square\" src=\"https://sessionize.com/image/e157.jpg\" alt=\"Harny Otuoniyo\"/>",
            "<div class=\"p-3\"><h4 class=\"text-sm md:text-lg font-bold\">Harny Otuoniyo</h4>",
            "<p class=\"text-xs md:text-sm mt-1 line-clamp-2\">Android Engineer at getyourguide </p></div></a>",
            "<a class=\"group flex flex-col h-full\" href=\"/sessions/b\">",
            "<img class=\"w-full aspect-square\" src=\"https://sessionize.com/image/afcb.jpg\" alt=\"Dinoy Raj\"/>",
            "<div class=\"p-3\"><h4 class=\"text-sm md:text-lg font-bold\">Dinoy Raj</h4>",
            "<p class=\"text-xs md:text-sm mt-1\">Product Engineer &#8211; Android @ Strollby | Uganda &#x27;25 &amp; Abu Dhabi</p></div></a>",
            "<a><div><h4>Title-Only Speaker</h4></div></a>",
            "</div></body></html>"
        );
        let speakers = extract_speakers(html);
        assert_eq!(speakers.len(), 3, "got {speakers:?}");
        assert_eq!(speakers[0].name, "Harny Otuoniyo");
        assert_eq!(
            speakers[0].role.as_deref(),
            Some("Android Engineer at getyourguide")
        );
        // Entities decoded, whitespace folded, trailing spaces gone.
        assert_eq!(
            speakers[1].role.as_deref(),
            Some("Product Engineer \u{2013} Android @ Strollby | Uganda '25 & Abu Dhabi")
        );
        // A card without a following <p> still yields the name.
        assert_eq!(speakers[2].name, "Title-Only Speaker");
        assert_eq!(speakers[2].role, None);
    }

    #[test]
    fn speaker_scan_skips_decoys_and_never_loops() {
        // `<PRE>` before a real role `<p>`, a longer tag starting with `<p`,
        // an empty and an uppercase H4 — none may wedge or dupe the scanner.
        let html = "<p class=ghost>x</p>".to_owned()
            + "<H4 CLASS=upper>Zed M</H4><PRE>not a paragraph</PRE><p>real role</p>"
            + "<h4></h4><p>orphan</p>"
            + "<h4>Fin</h4>";
        let speakers = extract_speakers(&html);
        assert_eq!(speakers.len(), 2, "got {speakers:?}");
        assert_eq!(speakers[0].name, "Zed M");
        assert_eq!(speakers[0].role.as_deref(), Some("real role"));
        assert_eq!(speakers[1].name, "Fin");
        assert_eq!(speakers[1].role, None);
    }

    #[test]
    fn non_speaker_pages_yield_no_speakers() {
        assert!(extract_speakers("<html><body><h1>Talk</h1><p>abstract</p></body></html>").is_empty());
    }
}

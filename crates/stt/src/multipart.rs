//! Minimal `multipart/form-data` parsing for the one client that talks to this
//! server: whispy-daemon's `SttClient`, which sends a few text fields and one
//! WAV file. Not a general-purpose parser (no nested parts, no transfer
//! encodings), but strict about the framing it does accept.

/// One form part: its field name and raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub data: Vec<u8>,
}

/// Extract the boundary from a `Content-Type: multipart/form-data; boundary=…` value.
pub fn boundary(content_type: &str) -> Option<String> {
    let (kind, params) = content_type.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| v.trim().trim_matches('"').to_string())
            .filter(|b| !b.is_empty())
    })
}

/// Split `body` into its parts. Parts without a `name` are skipped.
pub fn parse(body: &[u8], boundary: &str) -> Result<Vec<Part>, String> {
    let delim = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut rest = match find(body, &delim) {
        Some(i) => &body[i + delim.len()..],
        None => return Err("no multipart boundary in body".into()),
    };
    loop {
        if rest.starts_with(b"--") {
            return Ok(parts); // closing delimiter
        }
        rest = rest
            .strip_prefix(b"\r\n")
            .ok_or("malformed multipart delimiter line")?;
        let header_end = find(rest, b"\r\n\r\n").ok_or("multipart part without headers end")?;
        let headers = String::from_utf8_lossy(&rest[..header_end]);
        let after_headers = &rest[header_end + 4..];
        // The part's data ends right before "\r\n--boundary".
        let mut next_delim = b"\r\n".to_vec();
        next_delim.extend_from_slice(&delim);
        let data_end = find(after_headers, &next_delim).ok_or("unterminated multipart part")?;
        if let Some(name) = field_name(&headers) {
            parts.push(Part {
                name,
                data: after_headers[..data_end].to_vec(),
            });
        }
        rest = &after_headers[data_end + next_delim.len()..];
    }
}

/// The `name="…"` of a part's `Content-Disposition` header.
fn field_name(headers: &str) -> Option<String> {
    headers
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-disposition:"))?
        .split(';')
        .find_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            (k.trim() == "name").then(|| v.trim().trim_matches('"').to_string())
        })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape whispy-daemon's `stt::build_multipart` sends.
    fn daemon_body() -> Vec<u8> {
        let b = "whispy-boundary-7MA4YWxkTrZu0gW";
        let mut body = Vec::new();
        for (k, v) in [("response_format", "verbose_json"), ("language", "it")] {
            body.extend_from_slice(
                format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                    .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\n\
                 Content-Type: audio/wav\r\n\r\n"
            )
            .as_bytes(),
        );
        // Binary payload that itself contains CRLFs and dashes.
        body.extend_from_slice(b"RIFF\r\n--not-a-boundary\x00\xff");
        body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
        body
    }

    #[test]
    fn boundary_from_content_type() {
        assert_eq!(
            boundary("multipart/form-data; boundary=abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            boundary("Multipart/Form-Data; charset=utf-8; boundary=\"q\"").as_deref(),
            Some("q")
        );
        assert_eq!(boundary("application/json"), None);
        assert_eq!(boundary("multipart/form-data"), None);
    }

    #[test]
    fn parses_the_daemons_request() {
        let parts = parse(&daemon_body(), "whispy-boundary-7MA4YWxkTrZu0gW").unwrap();
        let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["response_format", "language", "file"]);
        assert_eq!(parts[1].data, b"it");
        assert_eq!(parts[2].data, b"RIFF\r\n--not-a-boundary\x00\xff");
    }

    #[test]
    fn rejects_bodies_without_framing() {
        parse(b"no boundary here", "x").unwrap_err();
        parse(
            b"--x\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nunterminated",
            "x",
        )
        .unwrap_err();
    }
}

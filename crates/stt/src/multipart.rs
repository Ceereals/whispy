//! Minimal `multipart/form-data` parsing for the one client that talks to this
//! server: whispy-daemon's `SttClient`, which sends the shared
//! [`whispy_common::inference`] request (a few text fields and one WAV file).
//! Not a general-purpose parser (no nested parts, no transfer encodings), but
//! strict about the framing it does accept.

use whispy_common::inference::field;

/// One form part: its field name and raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    name: String,
    data: Vec<u8>,
}

/// The `/inference` request fields whispy-stt acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    /// The uploaded WAV clip (`file`).
    pub wav: Vec<u8>,
    /// The requested `language`, if the field was sent.
    pub language: Option<String>,
}

/// Parse an `/inference` body and pick out its [`Form`]; the first part of a
/// name wins, other fields (`response_format`, `temperature`, `prompt`) are
/// ignored. Errors on bad framing or a missing `file` part.
pub fn inference_form(body: &[u8], boundary: &str) -> Result<Form, String> {
    let mut wav = None;
    let mut language = None;
    for part in parse(body, boundary)? {
        match part.name.as_str() {
            field::FILE if wav.is_none() => wav = Some(part.data),
            field::LANGUAGE if language.is_none() => {
                language = Some(String::from_utf8_lossy(&part.data).into_owned());
            }
            _ => {}
        }
    }
    let wav = wav.ok_or_else(|| format!("missing the `{}` part", field::FILE))?;
    Ok(Form { wav, language })
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
fn parse(body: &[u8], boundary: &str) -> Result<Vec<Part>, String> {
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
    use whispy_common::inference::{BOUNDARY, Request, build_multipart};

    /// Binary payload that itself contains CRLFs and dashes.
    const WAV: &[u8] = b"RIFF\r\n--not-a-boundary\x00\xff\r\n--";

    /// The exact body whispy-daemon sends, from the shared encoder.
    fn daemon_body(prompt: Option<&str>) -> Vec<u8> {
        build_multipart(
            BOUNDARY,
            &Request {
                language: "it",
                prompt,
            },
            WAV,
        )
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
        let parts = parse(&daemon_body(Some("Hyprland")), BOUNDARY).unwrap();
        let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "response_format",
                "language",
                "temperature",
                "prompt",
                "file"
            ]
        );
        assert_eq!(parts[0].data, b"verbose_json");
        assert_eq!(parts[2].data, b"0.0");
        assert_eq!(parts[3].data, b"Hyprland");
        assert_eq!(parts[4].data, WAV);

        let names: Vec<String> = parse(&daemon_body(None), BOUNDARY)
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(
            names,
            ["response_format", "language", "temperature", "file"]
        );
    }

    #[test]
    fn extracts_the_inference_form() {
        let form = inference_form(&daemon_body(Some("Hyprland")), BOUNDARY).unwrap();
        assert_eq!(
            form,
            Form {
                wav: WAV.to_vec(),
                language: Some("it".into()),
            }
        );
    }

    #[test]
    fn form_needs_a_file_part() {
        let body =
            b"--x\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\nit\r\n--x--\r\n";
        let err = inference_form(body, "x").unwrap_err();
        assert!(err.contains("`file`"), "{err}");
        let form = inference_form(
            b"--x\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nW\r\n--x--\r\n",
            "x",
        )
        .unwrap();
        assert_eq!(form.language, None);
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

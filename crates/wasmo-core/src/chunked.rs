//! `aws-chunked` request bodies: chunk framing with optional chunk
//! signatures and trailing headers (e.g. `x-amz-checksum-crc32`).

use crate::wire::Request;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub payload: Vec<u8>,
    pub trailers: Vec<(String, String)>,
}

fn line(body: &[u8], pos: &mut usize) -> Result<String, String> {
    let rest = &body[*pos..];
    let end = rest
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or("aws-chunked: missing CRLF")?;
    *pos += end + 2;
    String::from_utf8(rest[..end].to_vec()).map_err(|_| "aws-chunked: non-UTF-8 framing".into())
}

pub fn decode(body: &[u8]) -> Result<Decoded, String> {
    let mut pos = 0;
    let mut payload = Vec::new();
    loop {
        let header = line(body, &mut pos)?;
        let size = header.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| format!("aws-chunked: invalid chunk size {size:?}"))?;
        if size == 0 {
            break;
        }
        let chunk = body
            .get(pos..pos + size)
            .ok_or("aws-chunked: truncated chunk")?;
        payload.extend_from_slice(chunk);
        pos += size;
        if body.get(pos..pos + 2) != Some(b"\r\n") {
            return Err("aws-chunked: missing CRLF after chunk".into());
        }
        pos += 2;
    }
    let mut trailers = Vec::new();
    while pos < body.len() {
        let l = line(body, &mut pos)?;
        if l.is_empty() {
            break;
        }
        let (name, value) = l
            .split_once(':')
            .ok_or_else(|| format!("aws-chunked: invalid trailer {l:?}"))?;
        let name = name.trim().to_ascii_lowercase();
        if name != "x-amz-trailer-signature" {
            trailers.push((name, value.trim().to_string()));
        }
    }
    Ok(Decoded { payload, trailers })
}

pub fn is_chunked(req: &Request) -> bool {
    req.header("content-encoding")
        .is_some_and(|v| v.split(',').any(|e| e.trim() == "aws-chunked"))
        || req
            .header("x-amz-content-sha256")
            .is_some_and(|v| v.starts_with("STREAMING-"))
}

/// Replaces an `aws-chunked` body with its payload and moves the trailers
/// into the headers.
pub fn unwrap(req: &mut Request) -> Result<(), String> {
    let decoded = decode(&req.body)?;
    if let Some(expected) = req.header("x-amz-decoded-content-length")
        && expected.trim().parse::<usize>().ok() != Some(decoded.payload.len())
    {
        return Err(format!(
            "aws-chunked: decoded {} bytes but x-amz-decoded-content-length is {expected}",
            decoded.payload.len()
        ));
    }
    req.body = decoded.payload;
    req.headers.retain_mut(|(k, v)| {
        if k != "content-encoding" {
            return true;
        }
        *v = v
            .split(',')
            .map(str::trim)
            .filter(|e| *e != "aws-chunked" && !e.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        !v.is_empty()
    });
    req.headers.extend(decoded.trailers);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_unsigned_chunks_with_trailer() {
        let body = b"5\r\nhello\r\n6\r\n world\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n";
        let d = decode(body).unwrap();
        assert_eq!(d.payload, b"hello world");
        assert_eq!(
            d.trailers,
            vec![("x-amz-checksum-crc32".to_string(), "DUoRhQ==".to_string())]
        );
    }

    #[test]
    fn decodes_signed_chunks_and_drops_trailer_signature() {
        let body = b"3;chunk-signature=abc\r\nfoo\r\n0;chunk-signature=def\r\nx-amz-checksum-sha256:xyz\r\nx-amz-trailer-signature:123\r\n\r\n";
        let d = decode(body).unwrap();
        assert_eq!(d.payload, b"foo");
        assert_eq!(d.trailers.len(), 1);
        assert_eq!(d.trailers[0].0, "x-amz-checksum-sha256");
    }

    #[test]
    fn binary_chunk_may_contain_crlf() {
        let body = b"4\r\n\r\n\x00\xff\r\n0\r\n\r\n";
        assert_eq!(decode(body).unwrap().payload, b"\r\n\x00\xff");
    }

    #[test]
    fn rejects_truncated_bodies() {
        assert!(decode(b"a\r\nshort\r\n0\r\n\r\n").is_err());
        assert!(decode(b"zz\r\n").is_err());
    }

    #[test]
    fn unwrap_strips_encoding_and_appends_trailers() {
        let mut req = Request {
            method: "PUT".into(),
            path: "/b/k".into(),
            query: String::new(),
            headers: vec![
                ("content-encoding".into(), "aws-chunked, gzip".into()),
                ("x-amz-decoded-content-length".into(), "2".into()),
            ],
            body: b"2\r\nhi\r\n0\r\nx-amz-checksum-crc32:abc\r\n\r\n".to_vec(),
        };
        assert!(is_chunked(&req));
        unwrap(&mut req).unwrap();
        assert_eq!(req.body, b"hi");
        assert_eq!(req.header("content-encoding"), Some("gzip"));
        assert_eq!(req.header("x-amz-checksum-crc32"), Some("abc"));
    }
}

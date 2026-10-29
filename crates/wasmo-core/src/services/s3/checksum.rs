use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crc::{CRC_32_ISCSI, CRC_32_ISO_HDLC, CRC_64_NVME, Crc, Table};
use md5::{Digest, Md5};
use sha1::Sha1;
use sha2::{Sha256, Sha512};

const CRC32: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISO_HDLC);
const CRC32C: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISCSI);
const CRC64NVME: Crc<u64, Table<16>> = Crc::<u64, Table<16>>::new(&CRC_64_NVME);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Crc32,
    Crc32c,
    Crc64Nvme,
    Sha1,
    Sha256,
    Sha512,
}

impl Algorithm {
    pub const ALL: [Algorithm; 6] = [
        Algorithm::Crc32,
        Algorithm::Crc32c,
        Algorithm::Crc64Nvme,
        Algorithm::Sha1,
        Algorithm::Sha256,
        Algorithm::Sha512,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Crc32 => "CRC32",
            Algorithm::Crc32c => "CRC32C",
            Algorithm::Crc64Nvme => "CRC64NVME",
            Algorithm::Sha1 => "SHA1",
            Algorithm::Sha256 => "SHA256",
            Algorithm::Sha512 => "SHA512",
        }
    }

    /// Member name in the operation shapes, e.g. `ChecksumCRC32`.
    pub fn member(self) -> &'static str {
        match self {
            Algorithm::Crc32 => "ChecksumCRC32",
            Algorithm::Crc32c => "ChecksumCRC32C",
            Algorithm::Crc64Nvme => "ChecksumCRC64NVME",
            Algorithm::Sha1 => "ChecksumSHA1",
            Algorithm::Sha256 => "ChecksumSHA256",
            Algorithm::Sha512 => "ChecksumSHA512",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|a| a.name().eq_ignore_ascii_case(s))
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Algorithm::Crc32 => CRC32.checksum(data).to_be_bytes().to_vec(),
            Algorithm::Crc32c => CRC32C.checksum(data).to_be_bytes().to_vec(),
            Algorithm::Crc64Nvme => CRC64NVME.checksum(data).to_be_bytes().to_vec(),
            Algorithm::Sha1 => Sha1::digest(data).to_vec(),
            Algorithm::Sha256 => Sha256::digest(data).to_vec(),
            Algorithm::Sha512 => Sha512::digest(data).to_vec(),
        }
    }

    pub fn encode(self, data: &[u8]) -> String {
        STANDARD.encode(self.digest(data))
    }

    /// Whether a multipart upload defaults to a checksum of the whole object.
    pub fn full_object_by_default(self) -> bool {
        self == Algorithm::Crc64Nvme
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub algorithm: Algorithm,
    pub value: String,
    pub full_object: bool,
}

impl Checksum {
    pub fn of(algorithm: Algorithm, data: &[u8]) -> Self {
        Self {
            algorithm,
            value: algorithm.encode(data),
            full_object: true,
        }
    }

    pub fn kind(&self) -> &'static str {
        if self.full_object {
            "FULL_OBJECT"
        } else {
            "COMPOSITE"
        }
    }
}

/// Checksum of the concatenated part checksums, suffixed with the part count.
pub fn composite(algorithm: Algorithm, parts: &[&str]) -> String {
    let mut raw = Vec::new();
    for p in parts {
        raw.extend(STANDARD.decode(p).unwrap_or_default());
    }
    format!("{}-{}", algorithm.encode(&raw), parts.len())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn md5_hex(data: &[u8]) -> String {
    hex(&Md5::digest(data))
}

/// Multipart ETag: MD5 of the concatenated binary part MD5s, then `-<count>`.
pub fn multipart_etag(part_etags: &[&str]) -> String {
    let mut md5s = Vec::with_capacity(part_etags.len() * 16);
    for e in part_etags {
        let h = e.trim_matches('"');
        for i in (0..h.len()).step_by(2) {
            md5s.push(u8::from_str_radix(&h[i..i + 2], 16).unwrap_or_default());
        }
    }
    format!("\"{}-{}\"", md5_hex(&md5s), part_etags.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(Algorithm::Crc32.encode(b"hello world"), "DUoRhQ==");
        assert_eq!(Algorithm::Crc32c.encode(b"hello world"), "yZRlqg==");
        assert_eq!(
            hex(&Algorithm::Crc64Nvme.digest(b"123456789")),
            "ae8b14860a799888"
        );
        assert_eq!(
            Algorithm::Sha256.encode(b""),
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
        assert_eq!(Algorithm::parse("crc32c"), Some(Algorithm::Crc32c));
    }

    #[test]
    fn multipart_etag_is_md5_of_md5s() {
        let a = format!("\"{}\"", md5_hex(b"a"));
        let b = format!("\"{}\"", md5_hex(b"b"));
        let mut joined = Md5::digest(b"a").to_vec();
        joined.extend(Md5::digest(b"b"));
        assert_eq!(
            multipart_etag(&[&a, &b]),
            format!("\"{}-2\"", md5_hex(&joined))
        );
    }

    #[test]
    fn composite_checksum_counts_parts() {
        let p1 = Algorithm::Crc32.encode(b"a");
        let p2 = Algorithm::Crc32.encode(b"b");
        let mut raw = Algorithm::Crc32.digest(b"a");
        raw.extend(Algorithm::Crc32.digest(b"b"));
        assert_eq!(
            composite(Algorithm::Crc32, &[&p1, &p2]),
            format!("{}-2", Algorithm::Crc32.encode(&raw))
        );
    }
}

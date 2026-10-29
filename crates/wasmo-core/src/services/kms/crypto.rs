use std::convert::Infallible;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Nonce, Payload};
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::Generate;
use p256::pkcs8::{DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use rand_core::{TryCryptoRng, TryRng};
use rsa::{Oaep, Pkcs1v15Sign, Pss, RsaPrivateKey};
use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

use crate::env::Env;

pub struct EnvRng<'a>(pub &'a mut dyn Env);

impl TryRng for EnvRng<'_> {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        let mut b = [0u8; 4];
        self.0.random_fill(&mut b);
        Ok(u32::from_le_bytes(b))
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        let mut b = [0u8; 8];
        self.0.random_fill(&mut b);
        Ok(u64::from_le_bytes(b))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        self.0.random_fill(dst);
        Ok(())
    }
}

impl TryCryptoRng for EnvRng<'_> {}

#[derive(Clone)]
pub enum Material {
    Symmetric(Vec<[u8; 32]>),
    Hmac(Vec<u8>),
    Ecc(Box<p256::SecretKey>),
    Rsa(Box<RsaPrivateKey>),
    Pending,
}

const ENVELOPE_VERSION: u8 = 1;
const NONCE_LEN: usize = 12;

pub fn random_array<const N: usize>(rng: &mut EnvRng<'_>) -> [u8; N] {
    let mut b = [0u8; N];
    rng.0.random_fill(&mut b);
    b
}

pub fn generate(spec: &str, rng: &mut EnvRng<'_>) -> Option<Material> {
    Some(match spec {
        "SYMMETRIC_DEFAULT" => Material::Symmetric(vec![random_array(rng)]),
        "HMAC_224" => Material::Hmac(random_array::<28>(rng).to_vec()),
        "HMAC_256" => Material::Hmac(random_array::<32>(rng).to_vec()),
        "HMAC_384" => Material::Hmac(random_array::<48>(rng).to_vec()),
        "HMAC_512" => Material::Hmac(random_array::<64>(rng).to_vec()),
        "ECC_NIST_P256" => Material::Ecc(Box::new(p256::SecretKey::generate_from_rng(rng))),
        "RSA_2048" => rsa_key(rng, 2048),
        "RSA_3072" => rsa_key(rng, 3072),
        "RSA_4096" => rsa_key(rng, 4096),
        _ => return None,
    })
}

#[inline(never)]
fn rsa_key(rng: &mut EnvRng<'_>, bits: usize) -> Material {
    Material::Rsa(Box::new(
        RsaPrivateKey::new(rng, bits).expect("RSA key generation succeeds"),
    ))
}

pub fn context_bytes<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<u8> {
    let mut pairs: Vec<(&str, &str)> = pairs.into_iter().collect();
    pairs.sort();
    let mut out = Vec::new();
    for (k, v) in pairs {
        for s in [k, v] {
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
    }
    out
}

fn envelope_header(key_id: &str, version: u32) -> Vec<u8> {
    let mut h = vec![ENVELOPE_VERSION, key_id.len() as u8];
    h.extend_from_slice(key_id.as_bytes());
    h.extend_from_slice(&version.to_be_bytes());
    h
}

pub fn seal(
    keys: &[[u8; 32]],
    key_id: &str,
    context: &[u8],
    plaintext: &[u8],
    rng: &mut EnvRng<'_>,
) -> Vec<u8> {
    let version = keys.len() - 1;
    let mut out = envelope_header(key_id, version as u32);
    let mut aad = out.clone();
    aad.extend_from_slice(context);
    let nonce: [u8; NONCE_LEN] = random_array(rng);
    let cipher = Aes256Gcm::new_from_slice(&keys[version]).expect("32-byte key");
    let ct = cipher
        .encrypt(
            &Nonce::<Aes256Gcm>::from(nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .expect("AES-GCM encryption succeeds");
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

pub fn envelope_key_id(blob: &[u8]) -> Option<&str> {
    let (&ver, rest) = blob.split_first()?;
    let (&len, rest) = rest.split_first()?;
    (ver == ENVELOPE_VERSION).then_some(())?;
    std::str::from_utf8(rest.get(..len as usize)?).ok()
}

pub fn open(keys: &[[u8; 32]], blob: &[u8], context: &[u8]) -> Option<Vec<u8>> {
    let key_id = envelope_key_id(blob)?;
    let header_len = 2 + key_id.len() + 4;
    let version = u32::from_be_bytes(blob.get(header_len - 4..header_len)?.try_into().ok()?);
    let key = keys.get(version as usize)?;
    let nonce: [u8; NONCE_LEN] = blob
        .get(header_len..header_len + NONCE_LEN)?
        .try_into()
        .ok()?;
    let mut aad = blob[..header_len].to_vec();
    aad.extend_from_slice(context);
    Aes256Gcm::new_from_slice(key)
        .ok()?
        .decrypt(
            &Nonce::<Aes256Gcm>::from(nonce),
            Payload {
                msg: &blob[header_len + NONCE_LEN..],
                aad: &aad,
            },
        )
        .ok()
}

pub fn hash_len(algorithm: &str) -> usize {
    if algorithm.ends_with("SHA_512") {
        64
    } else if algorithm.ends_with("SHA_384") {
        48
    } else {
        32
    }
}

pub fn digest(algorithm: &str, message: &[u8]) -> Vec<u8> {
    match hash_len(algorithm) {
        64 => Sha512::digest(message).to_vec(),
        48 => Sha384::digest(message).to_vec(),
        _ => Sha256::digest(message).to_vec(),
    }
}

pub fn signing_algorithms(spec: &str) -> &'static [&'static str] {
    match spec {
        "ECC_NIST_P256" => &["ECDSA_SHA_256"],
        s if s.starts_with("RSA_") => &[
            "RSASSA_PSS_SHA_256",
            "RSASSA_PSS_SHA_384",
            "RSASSA_PSS_SHA_512",
            "RSASSA_PKCS1_V1_5_SHA_256",
            "RSASSA_PKCS1_V1_5_SHA_384",
            "RSASSA_PKCS1_V1_5_SHA_512",
        ],
        _ => &[],
    }
}

pub fn encryption_algorithms(spec: &str) -> &'static [&'static str] {
    match spec {
        "SYMMETRIC_DEFAULT" => &["SYMMETRIC_DEFAULT"],
        s if s.starts_with("RSA_") => &["RSAES_OAEP_SHA_1", "RSAES_OAEP_SHA_256"],
        _ => &[],
    }
}

pub fn mac_algorithm(spec: &str) -> Option<&'static str> {
    Some(match spec {
        "HMAC_224" => "HMAC_SHA_224",
        "HMAC_256" => "HMAC_SHA_256",
        "HMAC_384" => "HMAC_SHA_384",
        "HMAC_512" => "HMAC_SHA_512",
        _ => return None,
    })
}

#[inline(never)]
fn rsa_sign(
    key: &RsaPrivateKey,
    algorithm: &str,
    digest: &[u8],
    rng: &mut EnvRng<'_>,
) -> Option<Vec<u8>> {
    let pss = algorithm.starts_with("RSASSA_PSS_");
    match (pss, hash_len(algorithm)) {
        (true, 64) => key.sign_with_rng(rng, Pss::<Sha512>::new(), digest),
        (true, 48) => key.sign_with_rng(rng, Pss::<Sha384>::new(), digest),
        (true, _) => key.sign_with_rng(rng, Pss::<Sha256>::new(), digest),
        (false, 64) => key.sign(Pkcs1v15Sign::new::<Sha512>(), digest),
        (false, 48) => key.sign(Pkcs1v15Sign::new::<Sha384>(), digest),
        (false, _) => key.sign(Pkcs1v15Sign::new::<Sha256>(), digest),
    }
    .ok()
}

#[inline(never)]
fn rsa_verify(key: &RsaPrivateKey, algorithm: &str, digest: &[u8], signature: &[u8]) -> bool {
    let public = key.as_public_key();
    let pss = algorithm.starts_with("RSASSA_PSS_");
    match (pss, hash_len(algorithm)) {
        (true, 64) => public.verify(Pss::<Sha512>::new(), digest, signature),
        (true, 48) => public.verify(Pss::<Sha384>::new(), digest, signature),
        (true, _) => public.verify(Pss::<Sha256>::new(), digest, signature),
        (false, 64) => public.verify(Pkcs1v15Sign::new::<Sha512>(), digest, signature),
        (false, 48) => public.verify(Pkcs1v15Sign::new::<Sha384>(), digest, signature),
        (false, _) => public.verify(Pkcs1v15Sign::new::<Sha256>(), digest, signature),
    }
    .is_ok()
}

pub fn sign(
    material: &Material,
    algorithm: &str,
    digest: &[u8],
    rng: &mut EnvRng<'_>,
) -> Option<Vec<u8>> {
    match material {
        Material::Ecc(secret) => {
            let sig: Signature = SigningKey::from(secret.as_ref())
                .sign_prehash(digest)
                .ok()?;
            Some(sig.to_der().as_bytes().to_vec())
        }
        Material::Rsa(key) => rsa_sign(key, algorithm, digest, rng),
        _ => None,
    }
}

pub fn verify(material: &Material, algorithm: &str, digest: &[u8], signature: &[u8]) -> bool {
    match material {
        Material::Ecc(secret) => Signature::from_der(signature).is_ok_and(|sig| {
            VerifyingKey::from(secret.public_key())
                .verify_prehash(digest, &sig)
                .is_ok()
        }),
        Material::Rsa(key) => rsa_verify(key, algorithm, digest, signature),
        _ => false,
    }
}

fn hmac_with<M: Mac + KeyInit>(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = <M as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

pub fn mac(algorithm: &str, key: &[u8], message: &[u8]) -> Vec<u8> {
    match algorithm {
        "HMAC_SHA_224" => hmac_with::<Hmac<Sha224>>(key, message),
        "HMAC_SHA_384" => hmac_with::<Hmac<Sha384>>(key, message),
        "HMAC_SHA_512" => hmac_with::<Hmac<Sha512>>(key, message),
        _ => hmac_with::<Hmac<Sha256>>(key, message),
    }
}

pub fn public_key_der(material: &Material) -> Option<Vec<u8>> {
    let doc = match material {
        Material::Ecc(secret) => secret.public_key().to_public_key_der().ok()?,
        Material::Rsa(key) => key.as_public_key().to_public_key_der().ok()?,
        _ => return None,
    };
    Some(doc.as_bytes().to_vec())
}

pub fn private_key_der(material: &Material) -> Option<Vec<u8>> {
    let doc = match material {
        Material::Ecc(secret) => secret.to_pkcs8_der().ok()?,
        Material::Rsa(key) => key.to_pkcs8_der().ok()?,
        _ => return None,
    };
    Some(doc.as_bytes().to_vec())
}

#[inline(never)]
pub fn rsa_encrypt(
    material: &Material,
    algorithm: &str,
    plaintext: &[u8],
    rng: &mut EnvRng<'_>,
) -> Option<Vec<u8>> {
    let Material::Rsa(key) = material else {
        return None;
    };
    let public = key.as_public_key();
    if algorithm == "RSAES_OAEP_SHA_1" {
        public.encrypt(rng, Oaep::<Sha1>::new(), plaintext)
    } else {
        public.encrypt(rng, Oaep::<Sha256>::new(), plaintext)
    }
    .ok()
}

#[inline(never)]
pub fn rsa_decrypt(material: &Material, algorithm: &str, ciphertext: &[u8]) -> Option<Vec<u8>> {
    let Material::Rsa(key) = material else {
        return None;
    };
    if algorithm == "RSAES_OAEP_SHA_1" {
        key.decrypt(Oaep::<Sha1>::new(), ciphertext)
    } else {
        key.decrypt(Oaep::<Sha256>::new(), ciphertext)
    }
    .ok()
}

pub fn ecdh(material: &Material, peer_der: &[u8]) -> Option<Vec<u8>> {
    let Material::Ecc(secret) = material else {
        return None;
    };
    let peer = p256::PublicKey::from_public_key_der(peer_der).ok()?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), peer.as_affine());
    Some(shared.raw_secret_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::FixedEnv;

    fn env() -> FixedEnv {
        FixedEnv {
            now_ms: 0.0,
            seed: 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn symmetric(rng: &mut EnvRng<'_>) -> Vec<[u8; 32]> {
        match generate("SYMMETRIC_DEFAULT", rng) {
            Some(Material::Symmetric(keys)) => keys,
            _ => unreachable!(),
        }
    }

    #[test]
    fn envelope_round_trips_and_names_the_key() {
        let mut e = env();
        let mut rng = EnvRng(&mut e);
        let keys = symmetric(&mut rng);
        let ctx = context_bytes([("purpose", "test"), ("app", "wasmo")]);
        let blob = seal(&keys, "key-1", &ctx, b"secret", &mut rng);
        assert_eq!(envelope_key_id(&blob), Some("key-1"));
        assert_eq!(open(&keys, &blob, &ctx).as_deref(), Some(&b"secret"[..]));
        let reordered = context_bytes([("app", "wasmo"), ("purpose", "test")]);
        assert_eq!(
            open(&keys, &blob, &reordered).as_deref(),
            Some(&b"secret"[..])
        );
    }

    #[test]
    fn envelope_is_bound_to_context_and_key() {
        let mut e = env();
        let mut rng = EnvRng(&mut e);
        let keys = symmetric(&mut rng);
        let other = symmetric(&mut rng);
        let ctx = context_bytes([("a", "1")]);
        let blob = seal(&keys, "key-1", &ctx, b"secret", &mut rng);
        assert!(open(&keys, &blob, &context_bytes([("a", "2")])).is_none());
        assert!(open(&keys, &blob, &[]).is_none());
        assert!(open(&other, &blob, &ctx).is_none());
    }

    #[test]
    fn envelope_detects_tampering() {
        let mut e = env();
        let mut rng = EnvRng(&mut e);
        let keys = symmetric(&mut rng);
        let blob = seal(&keys, "key-1", &[], b"secret", &mut rng);
        for i in 0..blob.len() {
            let mut t = blob.clone();
            t[i] ^= 0x01;
            assert!(open(&keys, &t, &[]).is_none(), "byte {i}");
        }
        assert!(open(&keys, &blob[..blob.len() - 1], &[]).is_none());
    }

    #[test]
    fn envelope_survives_rotation() {
        let mut e = env();
        let mut rng = EnvRng(&mut e);
        let mut keys = symmetric(&mut rng);
        let old = seal(&keys, "k", &[], b"before", &mut rng);
        keys.push(random_array(&mut rng));
        let new = seal(&keys, "k", &[], b"after", &mut rng);
        assert_eq!(open(&keys, &old, &[]).as_deref(), Some(&b"before"[..]));
        assert_eq!(open(&keys, &new, &[]).as_deref(), Some(&b"after"[..]));
        assert!(open(&keys[..1], &new, &[]).is_none());
    }

    #[test]
    fn ecdsa_signs_and_verifies() {
        let mut e = env();
        let mut rng = EnvRng(&mut e);
        let key = generate("ECC_NIST_P256", &mut rng).unwrap();
        let d = digest("ECDSA_SHA_256", b"message");
        let sig = sign(&key, "ECDSA_SHA_256", &d, &mut rng).unwrap();
        assert!(verify(&key, "ECDSA_SHA_256", &d, &sig));
        let other = digest("ECDSA_SHA_256", b"messagf");
        assert!(!verify(&key, "ECDSA_SHA_256", &other, &sig));
        let spki = public_key_der(&key).unwrap();
        let vk = VerifyingKey::from_public_key_der(&spki).unwrap();
        let parsed = Signature::from_der(&sig).unwrap();
        assert!(vk.verify_prehash(&d, &parsed).is_ok());
    }

    #[test]
    fn rsa_signs_verifies_and_encrypts() {
        let mut e = env();
        let mut rng = EnvRng(&mut e);
        let key = generate("RSA_2048", &mut rng).unwrap();
        for alg in signing_algorithms("RSA_2048") {
            let d = digest(alg, b"message");
            let sig = sign(&key, alg, &d, &mut rng).unwrap();
            assert_eq!(sig.len(), 256);
            assert!(verify(&key, alg, &d, &sig), "{alg}");
            let mut bad = sig.clone();
            bad[0] ^= 1;
            assert!(!verify(&key, alg, &d, &bad), "{alg}");
        }
        for alg in encryption_algorithms("RSA_2048") {
            let ct = rsa_encrypt(&key, alg, b"hello", &mut rng).unwrap();
            assert_eq!(rsa_decrypt(&key, alg, &ct).as_deref(), Some(&b"hello"[..]));
        }
        assert!(public_key_der(&key).unwrap().len() > 256);
    }

    #[test]
    fn hmac_matches_rfc4231() {
        let key = [0x0bu8; 20];
        let tag = mac("HMAC_SHA_256", &key, b"Hi There");
        let hex: String = tag.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(mac("HMAC_SHA_224", &key, b"x").len(), 28);
        assert_eq!(mac("HMAC_SHA_512", &key, b"x").len(), 64);
    }

    #[test]
    fn ecdh_agrees_between_two_keys() {
        let mut e = env();
        let mut rng = EnvRng(&mut e);
        let a = generate("ECC_NIST_P256", &mut rng).unwrap();
        let b = generate("ECC_NIST_P256", &mut rng).unwrap();
        let ab = ecdh(&a, &public_key_der(&b).unwrap()).unwrap();
        let ba = ecdh(&b, &public_key_der(&a).unwrap()).unwrap();
        assert_eq!(ab, ba);
        assert_eq!(ab.len(), 32);
    }
}

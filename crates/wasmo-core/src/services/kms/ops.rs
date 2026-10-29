use serde_json::{Value, json};

use super::crypto::{self, EnvRng, Material};
use super::{
    Kms, b64, blob, encryption_context, err, opt_str, ranged, required, sized_blob, validation,
};
use crate::codec::AwsError;
use crate::service::{Ctx, OpResult};

const MAX_MESSAGE: usize = 4096;

fn invalid_ciphertext() -> AwsError {
    err(
        "InvalidCiphertextException",
        "The ciphertext refers to a customer master key that does not exist, does not exist in this region, or you are not allowed to access, or the ciphertext is corrupted, missing, or otherwise invalid.",
    )
}

fn incorrect_key() -> AwsError {
    err(
        "IncorrectKeyException",
        "The key ID in the request does not identify a CMK that can perform this operation.",
    )
}

fn unsupported_algorithm(arn: &str, algorithm: &str) -> AwsError {
    err(
        "InvalidKeyUsageException",
        format!("{arn} does not support the algorithm {algorithm}."),
    )
}

fn message_digest(input: &Value, algorithm: &str) -> Result<Vec<u8>, AwsError> {
    let message = sized_blob(input, "Message", MAX_MESSAGE)?;
    match opt_str(input, "MessageType").unwrap_or("RAW") {
        "RAW" => Ok(crypto::digest(algorithm, &message)),
        "DIGEST" if message.len() == crypto::hash_len(algorithm) => Ok(message),
        "DIGEST" => Err(validation(format!(
            "Digest is invalid length for algorithm {algorithm}."
        ))),
        other => Err(validation(format!("Invalid MessageType {other}"))),
    }
}

struct Sealed {
    arn: String,
    blob: Vec<u8>,
    algorithm: String,
}

impl Kms {
    fn encrypt_with(
        &mut self,
        ctx: &mut Ctx<'_>,
        reference: &str,
        plaintext: &[u8],
        context: &[u8],
        algorithm: Option<&str>,
    ) -> Result<Sealed, AwsError> {
        let algorithm = algorithm.unwrap_or("SYMMETRIC_DEFAULT").to_string();
        let key = self.key(ctx, reference)?;
        key.usable()?;
        key.require_usage("ENCRYPT_DECRYPT")?;
        if !crypto::encryption_algorithms(&key.spec).contains(&algorithm.as_str()) {
            return Err(unsupported_algorithm(&key.arn, &algorithm));
        }
        let mut rng = EnvRng(&mut *ctx.env);
        let blob = match &key.material {
            Material::Symmetric(keys) => crypto::seal(keys, &key.id, context, plaintext, &mut rng),
            m => crypto::rsa_encrypt(m, &algorithm, plaintext, &mut rng).ok_or_else(|| {
                validation(format!(
                    "Plaintext is too long for {algorithm} with this key."
                ))
            })?,
        };
        Ok(Sealed {
            arn: key.arn.clone(),
            blob,
            algorithm,
        })
    }

    fn decrypt_with(
        &mut self,
        ctx: &mut Ctx<'_>,
        reference: Option<&str>,
        ciphertext: &[u8],
        context: &[u8],
        algorithm: Option<&str>,
    ) -> Result<Sealed, AwsError> {
        let algorithm = algorithm.unwrap_or("SYMMETRIC_DEFAULT").to_string();
        let named = match reference {
            Some(r) => Some(self.resolve(ctx, r)?),
            None => None,
        };
        let id = if algorithm == "SYMMETRIC_DEFAULT" {
            let embedded = crypto::envelope_key_id(ciphertext).ok_or_else(invalid_ciphertext)?;
            if named.as_deref().is_some_and(|n| n != embedded) {
                return Err(incorrect_key());
            }
            embedded.to_string()
        } else {
            named.ok_or_else(|| {
                validation(format!("KeyId is required to decrypt with {algorithm}."))
            })?
        };
        let key = self
            .region(ctx)
            .keys
            .get(&id)
            .ok_or_else(invalid_ciphertext)?;
        key.usable()?;
        key.require_usage("ENCRYPT_DECRYPT")?;
        if !crypto::encryption_algorithms(&key.spec).contains(&algorithm.as_str()) {
            return Err(incorrect_key());
        }
        let plaintext = match &key.material {
            Material::Symmetric(keys) => crypto::open(keys, ciphertext, context),
            m => crypto::rsa_decrypt(m, &algorithm, ciphertext),
        }
        .ok_or_else(invalid_ciphertext)?;
        Ok(Sealed {
            arn: key.arn.clone(),
            blob: plaintext,
            algorithm,
        })
    }

    pub(super) fn encrypt(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let plaintext = sized_blob(input, "Plaintext", MAX_MESSAGE)?;
        let context = encryption_context(input, "EncryptionContext")?;
        let s = self.encrypt_with(
            ctx,
            required(input, "KeyId")?,
            &plaintext,
            &context,
            opt_str(input, "EncryptionAlgorithm"),
        )?;
        Ok(json!({
            "CiphertextBlob": b64(&s.blob),
            "KeyId": s.arn,
            "EncryptionAlgorithm": s.algorithm,
        }))
    }

    pub(super) fn decrypt(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let ciphertext = blob(input, "CiphertextBlob")?;
        let context = encryption_context(input, "EncryptionContext")?;
        let s = self.decrypt_with(
            ctx,
            opt_str(input, "KeyId"),
            &ciphertext,
            &context,
            opt_str(input, "EncryptionAlgorithm"),
        )?;
        Ok(json!({
            "KeyId": s.arn,
            "Plaintext": b64(&s.blob),
            "EncryptionAlgorithm": s.algorithm,
        }))
    }

    pub(super) fn re_encrypt(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let ciphertext = blob(input, "CiphertextBlob")?;
        let destination = required(input, "DestinationKeyId")?;
        let source_context = encryption_context(input, "SourceEncryptionContext")?;
        let destination_context = encryption_context(input, "DestinationEncryptionContext")?;
        let source = self.decrypt_with(
            ctx,
            opt_str(input, "SourceKeyId"),
            &ciphertext,
            &source_context,
            opt_str(input, "SourceEncryptionAlgorithm"),
        )?;
        let dest = self.encrypt_with(
            ctx,
            destination,
            &source.blob,
            &destination_context,
            opt_str(input, "DestinationEncryptionAlgorithm"),
        )?;
        Ok(json!({
            "CiphertextBlob": b64(&dest.blob),
            "SourceKeyId": source.arn,
            "KeyId": dest.arn,
            "SourceEncryptionAlgorithm": source.algorithm,
            "DestinationEncryptionAlgorithm": dest.algorithm,
        }))
    }

    fn symmetric_key(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> Result<String, AwsError> {
        let key = self.input_key(ctx, input)?;
        if key.spec != "SYMMETRIC_DEFAULT" {
            return Err(err(
                "InvalidKeyUsageException",
                format!("{} is not a symmetric encryption key.", key.arn),
            ));
        }
        Ok(key.id.clone())
    }

    pub(super) fn generate_data_key(
        &mut self,
        ctx: &mut Ctx<'_>,
        input: &Value,
        with_plaintext: bool,
    ) -> OpResult {
        let len = match (
            opt_str(input, "KeySpec"),
            ranged(input, "NumberOfBytes", 1, 1024)?,
        ) {
            (Some("AES_256"), None) => 32,
            (Some("AES_128"), None) => 16,
            (None, Some(n)) => n as usize,
            (Some(spec), None) => return Err(validation(format!("Invalid KeySpec {spec}"))),
            _ => {
                return Err(validation(
                    "Please specify either number of bytes or key spec.",
                ));
            }
        };
        let context = encryption_context(input, "EncryptionContext")?;
        let id = self.symmetric_key(ctx, input)?;
        let mut plaintext = vec![0u8; len];
        ctx.random_bytes(&mut plaintext);
        let s = self.encrypt_with(ctx, &id, &plaintext, &context, None)?;
        let mut out = json!({"CiphertextBlob": b64(&s.blob), "KeyId": s.arn});
        if with_plaintext {
            out["Plaintext"] = b64(&plaintext);
        }
        Ok(out)
    }

    pub(super) fn generate_data_key_pair(
        &mut self,
        ctx: &mut Ctx<'_>,
        input: &Value,
        with_plaintext: bool,
    ) -> OpResult {
        let spec = required(input, "KeyPairSpec")?;
        if !matches!(spec, "ECC_NIST_P256" | "RSA_2048" | "RSA_3072" | "RSA_4096") {
            return Err(err(
                "UnsupportedOperationException",
                format!("wasmo does not implement KeyPairSpec {spec}."),
            ));
        }
        let context = encryption_context(input, "EncryptionContext")?;
        let id = self.symmetric_key(ctx, input)?;
        self.region(ctx).keys[&id].usable()?;
        let pair = crypto::generate(spec, &mut EnvRng(&mut *ctx.env)).expect("spec checked above");
        let private = crypto::private_key_der(&pair).expect("asymmetric pair");
        let public = crypto::public_key_der(&pair).expect("asymmetric pair");
        let s = self.encrypt_with(ctx, &id, &private, &context, None)?;
        let mut out = json!({
            "PrivateKeyCiphertextBlob": b64(&s.blob),
            "PublicKey": b64(&public),
            "KeyId": s.arn,
            "KeyPairSpec": spec,
        });
        if with_plaintext {
            out["PrivateKeyPlaintext"] = b64(&private);
        }
        Ok(out)
    }

    pub(super) fn sign(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let algorithm = required(input, "SigningAlgorithm")?;
        let digest = message_digest(input, algorithm)?;
        let key = self.input_key(ctx, input)?;
        key.usable()?;
        key.require_usage("SIGN_VERIFY")?;
        if !crypto::signing_algorithms(&key.spec).contains(&algorithm) {
            return Err(unsupported_algorithm(&key.arn, algorithm));
        }
        let signature = crypto::sign(
            &key.material,
            algorithm,
            &digest,
            &mut EnvRng(&mut *ctx.env),
        )
        .ok_or_else(|| err("KMSInternalException", "Signing failed."))?;
        Ok(json!({
            "KeyId": key.arn,
            "Signature": b64(&signature),
            "SigningAlgorithm": algorithm,
        }))
    }

    pub(super) fn verify(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let algorithm = required(input, "SigningAlgorithm")?;
        let digest = message_digest(input, algorithm)?;
        let signature = blob(input, "Signature")?;
        let key = self.input_key(ctx, input)?;
        key.usable()?;
        key.require_usage("SIGN_VERIFY")?;
        if !crypto::signing_algorithms(&key.spec).contains(&algorithm) {
            return Err(unsupported_algorithm(&key.arn, algorithm));
        }
        if !crypto::verify(&key.material, algorithm, &digest, &signature) {
            return Err(err(
                "KMSInvalidSignatureException",
                "The signature is invalid.",
            ));
        }
        Ok(json!({
            "KeyId": key.arn,
            "SignatureValid": true,
            "SigningAlgorithm": algorithm,
        }))
    }

    pub(super) fn get_public_key(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let key = self.input_key(ctx, input)?;
        key.usable()?;
        let Some(public) = crypto::public_key_der(&key.material) else {
            return Err(err(
                "UnsupportedOperationException",
                format!("{} is not an asymmetric key.", key.arn),
            ));
        };
        let mut out = json!({
            "KeyId": key.arn,
            "PublicKey": b64(&public),
            "CustomerMasterKeySpec": key.spec,
            "KeySpec": key.spec,
            "KeyUsage": key.usage,
        });
        match key.usage.as_str() {
            "SIGN_VERIFY" => {
                out["SigningAlgorithms"] = json!(crypto::signing_algorithms(&key.spec))
            }
            "ENCRYPT_DECRYPT" => {
                out["EncryptionAlgorithms"] = json!(crypto::encryption_algorithms(&key.spec))
            }
            _ => out["KeyAgreementAlgorithms"] = json!(["ECDH"]),
        }
        Ok(out)
    }

    fn mac_key(
        &mut self,
        ctx: &mut Ctx<'_>,
        input: &Value,
    ) -> Result<(String, String, Vec<u8>), AwsError> {
        let algorithm = required(input, "MacAlgorithm")?;
        let message = sized_blob(input, "Message", MAX_MESSAGE)?;
        let key = self.input_key(ctx, input)?;
        key.usable()?;
        key.require_usage("GENERATE_VERIFY_MAC")?;
        let Material::Hmac(secret) = &key.material else {
            unreachable!("GENERATE_VERIFY_MAC keys hold HMAC material");
        };
        if crypto::mac_algorithm(&key.spec) != Some(algorithm) {
            return Err(unsupported_algorithm(&key.arn, algorithm));
        }
        Ok((
            key.arn.clone(),
            algorithm.to_string(),
            crypto::mac(algorithm, secret, &message),
        ))
    }

    pub(super) fn generate_mac(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let (arn, algorithm, mac) = self.mac_key(ctx, input)?;
        Ok(json!({"Mac": b64(&mac), "MacAlgorithm": algorithm, "KeyId": arn}))
    }

    pub(super) fn verify_mac(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let given = blob(input, "Mac")?;
        let (arn, algorithm, mac) = self.mac_key(ctx, input)?;
        if given != mac {
            return Err(err("KMSInvalidMacException", "The MAC is invalid."));
        }
        Ok(json!({"KeyId": arn, "MacValid": true, "MacAlgorithm": algorithm}))
    }

    pub(super) fn derive_shared_secret(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let algorithm = required(input, "KeyAgreementAlgorithm")?;
        if algorithm != "ECDH" {
            return Err(validation(format!(
                "Invalid KeyAgreementAlgorithm {algorithm}"
            )));
        }
        let peer = blob(input, "PublicKey")?;
        let key = self.input_key(ctx, input)?;
        key.usable()?;
        key.require_usage("KEY_AGREEMENT")?;
        let secret = crypto::ecdh(&key.material, &peer).ok_or_else(|| {
            validation("PublicKey is not a DER-encoded SubjectPublicKeyInfo on the key's curve.")
        })?;
        Ok(json!({
            "KeyId": key.arn,
            "SharedSecret": b64(&secret),
            "KeyAgreementAlgorithm": "ECDH",
            "KeyOrigin": key.origin,
        }))
    }

    pub(super) fn generate_random(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let len = ranged(input, "NumberOfBytes", 1, 1024)?
            .ok_or_else(|| validation("NumberOfBytes is required."))?;
        let mut bytes = vec![0u8; len as usize];
        ctx.random_bytes(&mut bytes);
        Ok(json!({"Plaintext": b64(&bytes)}))
    }
}

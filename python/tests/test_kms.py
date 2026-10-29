import boto3


def test_encrypt_decrypt(aws):
    kms = boto3.client("kms")
    key_id = kms.create_key(Description="app")["KeyMetadata"]["KeyId"]
    kms.create_alias(AliasName="alias/app", TargetKeyId=key_id)
    blob = kms.encrypt(
        KeyId="alias/app", Plaintext=b"secret\x00bytes", EncryptionContext={"tenant": "a"}
    )["CiphertextBlob"]
    assert b"secret" not in blob
    out = kms.decrypt(CiphertextBlob=blob, EncryptionContext={"tenant": "a"})
    assert out["Plaintext"] == b"secret\x00bytes"
    assert out["KeyId"].endswith(key_id)


def test_wrong_context_fails(aws):
    kms = boto3.client("kms")
    key_id = kms.create_key()["KeyMetadata"]["KeyId"]
    blob = kms.encrypt(KeyId=key_id, Plaintext=b"x", EncryptionContext={"a": "1"})["CiphertextBlob"]
    try:
        kms.decrypt(CiphertextBlob=blob, EncryptionContext={"a": "2"})
    except kms.exceptions.InvalidCiphertextException:
        pass
    else:
        raise AssertionError("decrypt with the wrong context succeeded")


def test_data_key(aws):
    kms = boto3.client("kms")
    key_id = kms.create_key()["KeyMetadata"]["KeyId"]
    key = kms.generate_data_key(KeyId=key_id, KeySpec="AES_256")
    assert len(key["Plaintext"]) == 32
    assert kms.decrypt(CiphertextBlob=key["CiphertextBlob"])["Plaintext"] == key["Plaintext"]

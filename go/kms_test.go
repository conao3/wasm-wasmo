package wasmo_test

import (
	"bytes"
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/kms"
	"github.com/aws/aws-sdk-go-v2/service/kms/types"
)

func newKMS(t *testing.T) *kms.Client {
	t.Helper()
	return kms.NewFromConfig(newMock(t).Config())
}

func createKey(t *testing.T, client *kms.Client, in *kms.CreateKeyInput) *types.KeyMetadata {
	t.Helper()
	out, err := client.CreateKey(context.Background(), in)
	if err != nil {
		t.Fatal(err)
	}
	return out.KeyMetadata
}

func wantCode(t *testing.T, err error, code string) {
	t.Helper()
	if got := apiCode(err); got != code {
		t.Fatalf("error = %v (code %q), want %s", err, got, code)
	}
}

func TestKMSCreateDescribeAlias(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	key := createKey(t, client, &kms.CreateKeyInput{
		Description: aws.String("app key"),
		Tags:        []types.Tag{{TagKey: aws.String("env"), TagValue: aws.String("test")}},
	})
	if key.KeyState != types.KeyStateEnabled || key.KeySpec != types.KeySpecSymmetricDefault || key.KeyUsage != types.KeyUsageTypeEncryptDecrypt || !key.Enabled {
		t.Fatalf("metadata = %+v", key)
	}
	if _, err := client.CreateAlias(ctx, &kms.CreateAliasInput{AliasName: aws.String("alias/app"), TargetKeyId: key.KeyId}); err != nil {
		t.Fatal(err)
	}
	_, err := client.CreateAlias(ctx, &kms.CreateAliasInput{AliasName: aws.String("alias/app"), TargetKeyId: key.KeyId})
	wantCode(t, err, "AlreadyExistsException")
	_, err = client.CreateAlias(ctx, &kms.CreateAliasInput{AliasName: aws.String("alias/aws/s3"), TargetKeyId: key.KeyId})
	wantCode(t, err, "NotAuthorizedException")

	aliasArn := "arn:aws:kms:us-east-1:000000000000:alias/app"
	for _, ref := range []string{aws.ToString(key.KeyId), aws.ToString(key.Arn), "alias/app", aliasArn} {
		out, err := client.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: aws.String(ref)})
		if err != nil {
			t.Fatalf("%s: %v", ref, err)
		}
		if aws.ToString(out.KeyMetadata.KeyId) != aws.ToString(key.KeyId) || aws.ToString(out.KeyMetadata.Description) != "app key" {
			t.Fatalf("%s: metadata = %+v", ref, out.KeyMetadata)
		}
	}
	_, err = client.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: aws.String("alias/missing")})
	wantCode(t, err, "NotFoundException")
	_, err = client.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: aws.String("00000000-0000-4000-8000-000000000000")})
	wantCode(t, err, "NotFoundException")

	other := createKey(t, client, &kms.CreateKeyInput{})
	if _, err := client.UpdateAlias(ctx, &kms.UpdateAliasInput{AliasName: aws.String("alias/app"), TargetKeyId: other.KeyId}); err != nil {
		t.Fatal(err)
	}
	aliases, err := client.ListAliases(ctx, &kms.ListAliasesInput{KeyId: other.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	if len(aliases.Aliases) != 1 || aws.ToString(aliases.Aliases[0].AliasArn) != aliasArn {
		t.Fatalf("aliases = %+v", aliases.Aliases)
	}
	if _, err := client.DeleteAlias(ctx, &kms.DeleteAliasInput{AliasName: aws.String("alias/app")}); err != nil {
		t.Fatal(err)
	}
	_, err = client.DeleteAlias(ctx, &kms.DeleteAliasInput{AliasName: aws.String("alias/app")})
	wantCode(t, err, "NotFoundException")

	managed, err := client.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: aws.String("alias/aws/s3")})
	if err != nil {
		t.Fatal(err)
	}
	if managed.KeyMetadata.KeyManager != types.KeyManagerTypeAws {
		t.Fatalf("managed key = %+v", managed.KeyMetadata)
	}

	keys, err := client.ListKeys(ctx, &kms.ListKeysInput{Limit: aws.Int32(2)})
	if err != nil {
		t.Fatal(err)
	}
	if len(keys.Keys) != 2 || !keys.Truncated {
		t.Fatalf("first page = %+v", keys)
	}
	rest, err := client.ListKeys(ctx, &kms.ListKeysInput{Marker: keys.NextMarker})
	if err != nil {
		t.Fatal(err)
	}
	if len(rest.Keys) != 1 || rest.Truncated {
		t.Fatalf("second page = %+v", rest)
	}

	if _, err := client.TagResource(ctx, &kms.TagResourceInput{KeyId: key.KeyId, Tags: []types.Tag{{TagKey: aws.String("team"), TagValue: aws.String("core")}}}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.UntagResource(ctx, &kms.UntagResourceInput{KeyId: key.KeyId, TagKeys: []string{"env"}}); err != nil {
		t.Fatal(err)
	}
	tags, err := client.ListResourceTags(ctx, &kms.ListResourceTagsInput{KeyId: key.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	if len(tags.Tags) != 1 || aws.ToString(tags.Tags[0].TagKey) != "team" {
		t.Fatalf("tags = %+v", tags.Tags)
	}
	if _, err := client.UpdateKeyDescription(ctx, &kms.UpdateKeyDescriptionInput{KeyId: key.KeyId, Description: aws.String("renamed")}); err != nil {
		t.Fatal(err)
	}
	desc, _ := client.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: key.KeyId})
	if aws.ToString(desc.KeyMetadata.Description) != "renamed" {
		t.Fatalf("description = %q", aws.ToString(desc.KeyMetadata.Description))
	}
}

func TestKMSEncryptDecryptReEncrypt(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	a := createKey(t, client, &kms.CreateKeyInput{})
	b := createKey(t, client, &kms.CreateKeyInput{})
	ec := map[string]string{"tenant": "t1", "purpose": "test"}

	enc, err := client.Encrypt(ctx, &kms.EncryptInput{KeyId: a.KeyId, Plaintext: []byte("hello\x00world"), EncryptionContext: ec})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(enc.KeyId) != aws.ToString(a.Arn) || enc.EncryptionAlgorithm != types.EncryptionAlgorithmSpecSymmetricDefault {
		t.Fatalf("encrypt = %+v", enc)
	}
	if bytes.Contains(enc.CiphertextBlob, []byte("hello")) {
		t.Fatal("ciphertext contains the plaintext")
	}
	again, _ := client.Encrypt(ctx, &kms.EncryptInput{KeyId: a.KeyId, Plaintext: []byte("hello\x00world"), EncryptionContext: ec})
	if bytes.Equal(enc.CiphertextBlob, again.CiphertextBlob) {
		t.Fatal("two encryptions produced the same ciphertext")
	}

	dec, err := client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: enc.CiphertextBlob, EncryptionContext: map[string]string{"purpose": "test", "tenant": "t1"}})
	if err != nil {
		t.Fatal(err)
	}
	if string(dec.Plaintext) != "hello\x00world" || aws.ToString(dec.KeyId) != aws.ToString(a.Arn) {
		t.Fatalf("decrypt = %+v", dec)
	}

	_, err = client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: enc.CiphertextBlob, EncryptionContext: map[string]string{"tenant": "t2", "purpose": "test"}})
	wantCode(t, err, "InvalidCiphertextException")
	_, err = client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: enc.CiphertextBlob})
	wantCode(t, err, "InvalidCiphertextException")
	_, err = client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: enc.CiphertextBlob, EncryptionContext: ec, KeyId: b.KeyId})
	wantCode(t, err, "IncorrectKeyException")
	tampered := bytes.Clone(enc.CiphertextBlob)
	tampered[len(tampered)-1] ^= 1
	_, err = client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: tampered, EncryptionContext: ec})
	wantCode(t, err, "InvalidCiphertextException")
	_, err = client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: []byte("garbage")})
	wantCode(t, err, "InvalidCiphertextException")

	re, err := client.ReEncrypt(ctx, &kms.ReEncryptInput{
		CiphertextBlob:               enc.CiphertextBlob,
		SourceEncryptionContext:      ec,
		DestinationKeyId:             b.KeyId,
		DestinationEncryptionContext: map[string]string{"stage": "2"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(re.SourceKeyId) != aws.ToString(a.Arn) || aws.ToString(re.KeyId) != aws.ToString(b.Arn) {
		t.Fatalf("re-encrypt = %+v", re)
	}
	dec2, err := client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: re.CiphertextBlob, KeyId: b.Arn, EncryptionContext: map[string]string{"stage": "2"}})
	if err != nil {
		t.Fatal(err)
	}
	if string(dec2.Plaintext) != "hello\x00world" {
		t.Fatalf("plaintext = %q", dec2.Plaintext)
	}

	if _, err := client.RotateKeyOnDemand(ctx, &kms.RotateKeyOnDemandInput{KeyId: a.KeyId}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: enc.CiphertextBlob, EncryptionContext: ec}); err != nil {
		t.Fatalf("decrypt after rotation: %v", err)
	}
	rotations, err := client.ListKeyRotations(ctx, &kms.ListKeyRotationsInput{KeyId: a.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	if len(rotations.Rotations) != 1 || rotations.Rotations[0].RotationType != types.RotationTypeOnDemand {
		t.Fatalf("rotations = %+v", rotations.Rotations)
	}
}

func TestKMSDataKeys(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	key := createKey(t, client, &kms.CreateKeyInput{})
	ec := map[string]string{"file": "a.txt"}

	dk, err := client.GenerateDataKey(ctx, &kms.GenerateDataKeyInput{KeyId: key.KeyId, KeySpec: types.DataKeySpecAes256, EncryptionContext: ec})
	if err != nil {
		t.Fatal(err)
	}
	if len(dk.Plaintext) != 32 {
		t.Fatalf("plaintext length = %d", len(dk.Plaintext))
	}
	dec, err := client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: dk.CiphertextBlob, EncryptionContext: ec})
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(dec.Plaintext, dk.Plaintext) {
		t.Fatal("data key does not round-trip")
	}

	np, err := client.GenerateDataKeyWithoutPlaintext(ctx, &kms.GenerateDataKeyWithoutPlaintextInput{KeyId: key.KeyId, NumberOfBytes: aws.Int32(64)})
	if err != nil {
		t.Fatal(err)
	}
	dec, err = client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: np.CiphertextBlob})
	if err != nil {
		t.Fatal(err)
	}
	if len(dec.Plaintext) != 64 {
		t.Fatalf("plaintext length = %d", len(dec.Plaintext))
	}
	_, err = client.GenerateDataKey(ctx, &kms.GenerateDataKeyInput{KeyId: key.KeyId})
	wantCode(t, err, "ValidationException")

	pair, err := client.GenerateDataKeyPair(ctx, &kms.GenerateDataKeyPairInput{KeyId: key.KeyId, KeyPairSpec: types.DataKeyPairSpecEccNistP256})
	if err != nil {
		t.Fatal(err)
	}
	priv, err := x509.ParsePKCS8PrivateKey(pair.PrivateKeyPlaintext)
	if err != nil {
		t.Fatal(err)
	}
	pub, err := x509.ParsePKIXPublicKey(pair.PublicKey)
	if err != nil {
		t.Fatal(err)
	}
	if !priv.(*ecdsa.PrivateKey).PublicKey.Equal(pub) {
		t.Fatal("data key pair halves do not match")
	}
	unwrapped, err := client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: pair.PrivateKeyCiphertextBlob})
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(unwrapped.Plaintext, pair.PrivateKeyPlaintext) {
		t.Fatal("encrypted private key does not round-trip")
	}
	bare, err := client.GenerateDataKeyPairWithoutPlaintext(ctx, &kms.GenerateDataKeyPairWithoutPlaintextInput{KeyId: key.KeyId, KeyPairSpec: types.DataKeyPairSpecEccNistP256})
	if err != nil {
		t.Fatal(err)
	}
	if len(bare.PublicKey) == 0 || len(bare.PrivateKeyCiphertextBlob) == 0 {
		t.Fatalf("pair without plaintext = %+v", bare)
	}
}

func TestKMSSignVerifyECC(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	key := createKey(t, client, &kms.CreateKeyInput{KeySpec: types.KeySpecEccNistP256, KeyUsage: types.KeyUsageTypeSignVerify})
	if len(key.SigningAlgorithms) != 1 || key.SigningAlgorithms[0] != types.SigningAlgorithmSpecEcdsaSha256 {
		t.Fatalf("signing algorithms = %v", key.SigningAlgorithms)
	}
	pk, err := client.GetPublicKey(ctx, &kms.GetPublicKeyInput{KeyId: key.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := x509.ParsePKIXPublicKey(pk.PublicKey)
	if err != nil {
		t.Fatal(err)
	}
	pub := parsed.(*ecdsa.PublicKey)
	msg := []byte("sign me")
	digest := sha256.Sum256(msg)

	for _, in := range []*kms.SignInput{
		{KeyId: key.KeyId, Message: msg, SigningAlgorithm: types.SigningAlgorithmSpecEcdsaSha256},
		{KeyId: key.KeyId, Message: digest[:], MessageType: types.MessageTypeDigest, SigningAlgorithm: types.SigningAlgorithmSpecEcdsaSha256},
	} {
		sig, err := client.Sign(ctx, in)
		if err != nil {
			t.Fatal(err)
		}
		if !ecdsa.VerifyASN1(pub, digest[:], sig.Signature) {
			t.Fatalf("Go rejects the %s signature", in.MessageType)
		}
		v, err := client.Verify(ctx, &kms.VerifyInput{KeyId: key.KeyId, Message: msg, Signature: sig.Signature, SigningAlgorithm: types.SigningAlgorithmSpecEcdsaSha256})
		if err != nil || !v.SignatureValid {
			t.Fatalf("verify = %+v, %v", v, err)
		}
	}
	sig, _ := client.Sign(ctx, &kms.SignInput{KeyId: key.KeyId, Message: msg, SigningAlgorithm: types.SigningAlgorithmSpecEcdsaSha256})
	_, err = client.Verify(ctx, &kms.VerifyInput{KeyId: key.KeyId, Message: []byte("other"), Signature: sig.Signature, SigningAlgorithm: types.SigningAlgorithmSpecEcdsaSha256})
	wantCode(t, err, "KMSInvalidSignatureException")
	_, err = client.Sign(ctx, &kms.SignInput{KeyId: key.KeyId, Message: msg, SigningAlgorithm: types.SigningAlgorithmSpecRsassaPssSha256})
	wantCode(t, err, "InvalidKeyUsageException")
	_, err = client.Encrypt(ctx, &kms.EncryptInput{KeyId: key.KeyId, Plaintext: msg})
	wantCode(t, err, "InvalidKeyUsageException")
}

func TestKMSSignVerifyRSA(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	start := time.Now()
	key := createKey(t, client, &kms.CreateKeyInput{KeySpec: types.KeySpecRsa2048, KeyUsage: types.KeyUsageTypeSignVerify})
	t.Logf("RSA_2048 CreateKey took %s", time.Since(start))
	pk, err := client.GetPublicKey(ctx, &kms.GetPublicKeyInput{KeyId: key.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := x509.ParsePKIXPublicKey(pk.PublicKey)
	if err != nil {
		t.Fatal(err)
	}
	pub := parsed.(*rsa.PublicKey)
	if pub.N.BitLen() != 2048 {
		t.Fatalf("modulus bits = %d", pub.N.BitLen())
	}
	msg := []byte("sign me with rsa")
	digest := sha256.Sum256(msg)

	for _, alg := range []types.SigningAlgorithmSpec{types.SigningAlgorithmSpecRsassaPkcs1V15Sha256, types.SigningAlgorithmSpecRsassaPssSha256} {
		for _, mt := range []types.MessageType{types.MessageTypeRaw, types.MessageTypeDigest} {
			m := msg
			if mt == types.MessageTypeDigest {
				m = digest[:]
			}
			sig, err := client.Sign(ctx, &kms.SignInput{KeyId: key.KeyId, Message: m, MessageType: mt, SigningAlgorithm: alg})
			if err != nil {
				t.Fatal(err)
			}
			if alg == types.SigningAlgorithmSpecRsassaPssSha256 {
				err = rsa.VerifyPSS(pub, crypto.SHA256, digest[:], sig.Signature, &rsa.PSSOptions{SaltLength: rsa.PSSSaltLengthEqualsHash})
			} else {
				err = rsa.VerifyPKCS1v15(pub, crypto.SHA256, digest[:], sig.Signature)
			}
			if err != nil {
				t.Fatalf("%s/%s: Go rejects the signature: %v", alg, mt, err)
			}
			v, err := client.Verify(ctx, &kms.VerifyInput{KeyId: key.KeyId, Message: msg, Signature: sig.Signature, SigningAlgorithm: alg})
			if err != nil || !v.SignatureValid {
				t.Fatalf("%s/%s: verify = %+v, %v", alg, mt, v, err)
			}
		}
	}
	_, err = client.Sign(ctx, &kms.SignInput{KeyId: key.KeyId, Message: []byte("short"), MessageType: types.MessageTypeDigest, SigningAlgorithm: types.SigningAlgorithmSpecRsassaPssSha256})
	wantCode(t, err, "ValidationException")
}

func TestKMSRSAEncryption(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	key := createKey(t, client, &kms.CreateKeyInput{KeySpec: types.KeySpecRsa2048, KeyUsage: types.KeyUsageTypeEncryptDecrypt})
	pk, err := client.GetPublicKey(ctx, &kms.GetPublicKeyInput{KeyId: key.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	parsed, _ := x509.ParsePKIXPublicKey(pk.PublicKey)
	ct, err := rsa.EncryptOAEP(sha256.New(), bytes.NewReader(bytes.Repeat([]byte{7}, 512)), parsed.(*rsa.PublicKey), []byte("from go"), nil)
	if err != nil {
		t.Fatal(err)
	}
	dec, err := client.Decrypt(ctx, &kms.DecryptInput{KeyId: key.KeyId, CiphertextBlob: ct, EncryptionAlgorithm: types.EncryptionAlgorithmSpecRsaesOaepSha256})
	if err != nil {
		t.Fatal(err)
	}
	if string(dec.Plaintext) != "from go" {
		t.Fatalf("plaintext = %q", dec.Plaintext)
	}
	_, err = client.Encrypt(ctx, &kms.EncryptInput{KeyId: key.KeyId, Plaintext: []byte("x")})
	wantCode(t, err, "InvalidKeyUsageException")
}

func TestKMSMacAndRandom(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	key := createKey(t, client, &kms.CreateKeyInput{KeySpec: types.KeySpecHmac256, KeyUsage: types.KeyUsageTypeGenerateVerifyMac})
	mac, err := client.GenerateMac(ctx, &kms.GenerateMacInput{KeyId: key.KeyId, Message: []byte("payload"), MacAlgorithm: types.MacAlgorithmSpecHmacSha256})
	if err != nil {
		t.Fatal(err)
	}
	if len(mac.Mac) != 32 {
		t.Fatalf("mac length = %d", len(mac.Mac))
	}
	v, err := client.VerifyMac(ctx, &kms.VerifyMacInput{KeyId: key.KeyId, Message: []byte("payload"), Mac: mac.Mac, MacAlgorithm: types.MacAlgorithmSpecHmacSha256})
	if err != nil || !v.MacValid {
		t.Fatalf("verify mac = %+v, %v", v, err)
	}
	_, err = client.VerifyMac(ctx, &kms.VerifyMacInput{KeyId: key.KeyId, Message: []byte("tampered"), Mac: mac.Mac, MacAlgorithm: types.MacAlgorithmSpecHmacSha256})
	wantCode(t, err, "KMSInvalidMacException")
	_, err = client.GenerateMac(ctx, &kms.GenerateMacInput{KeyId: key.KeyId, Message: []byte("payload"), MacAlgorithm: types.MacAlgorithmSpecHmacSha512})
	wantCode(t, err, "InvalidKeyUsageException")

	for _, n := range []int32{1, 32, 1024} {
		r, err := client.GenerateRandom(ctx, &kms.GenerateRandomInput{NumberOfBytes: aws.Int32(n)})
		if err != nil {
			t.Fatal(err)
		}
		if len(r.Plaintext) != int(n) {
			t.Fatalf("random length = %d, want %d", len(r.Plaintext), n)
		}
	}
	_, err = client.GenerateRandom(ctx, &kms.GenerateRandomInput{NumberOfBytes: aws.Int32(1025)})
	wantCode(t, err, "ValidationException")
}

func TestKMSKeyStates(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	key := createKey(t, client, &kms.CreateKeyInput{})
	enc, err := client.Encrypt(ctx, &kms.EncryptInput{KeyId: key.KeyId, Plaintext: []byte("x")})
	if err != nil {
		t.Fatal(err)
	}
	state := func() types.KeyState {
		out, err := client.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: key.KeyId})
		if err != nil {
			t.Fatal(err)
		}
		return out.KeyMetadata.KeyState
	}

	if _, err := client.DisableKey(ctx, &kms.DisableKeyInput{KeyId: key.KeyId}); err != nil {
		t.Fatal(err)
	}
	if s := state(); s != types.KeyStateDisabled {
		t.Fatalf("state = %s", s)
	}
	_, err = client.Encrypt(ctx, &kms.EncryptInput{KeyId: key.KeyId, Plaintext: []byte("x")})
	wantCode(t, err, "DisabledException")
	_, err = client.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: enc.CiphertextBlob})
	wantCode(t, err, "DisabledException")
	if _, err := client.EnableKey(ctx, &kms.EnableKeyInput{KeyId: key.KeyId}); err != nil {
		t.Fatal(err)
	}
	if s := state(); s != types.KeyStateEnabled {
		t.Fatalf("state = %s", s)
	}

	_, err = client.ScheduleKeyDeletion(ctx, &kms.ScheduleKeyDeletionInput{KeyId: key.KeyId, PendingWindowInDays: aws.Int32(3)})
	wantCode(t, err, "ValidationException")
	sched, err := client.ScheduleKeyDeletion(ctx, &kms.ScheduleKeyDeletionInput{KeyId: key.KeyId, PendingWindowInDays: aws.Int32(7)})
	if err != nil {
		t.Fatal(err)
	}
	if sched.KeyState != types.KeyStatePendingDeletion || aws.ToInt32(sched.PendingWindowInDays) != 7 {
		t.Fatalf("schedule = %+v", sched)
	}
	if d := time.Until(aws.ToTime(sched.DeletionDate)); d < 6*24*time.Hour || d > 8*24*time.Hour {
		t.Fatalf("deletion date = %s", aws.ToTime(sched.DeletionDate))
	}
	desc, _ := client.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: key.KeyId})
	if desc.KeyMetadata.KeyState != types.KeyStatePendingDeletion || desc.KeyMetadata.DeletionDate == nil || desc.KeyMetadata.Enabled {
		t.Fatalf("metadata = %+v", desc.KeyMetadata)
	}
	_, err = client.Encrypt(ctx, &kms.EncryptInput{KeyId: key.KeyId, Plaintext: []byte("x")})
	wantCode(t, err, "KMSInvalidStateException")
	_, err = client.EnableKey(ctx, &kms.EnableKeyInput{KeyId: key.KeyId})
	wantCode(t, err, "KMSInvalidStateException")
	_, err = client.ScheduleKeyDeletion(ctx, &kms.ScheduleKeyDeletionInput{KeyId: key.KeyId})
	wantCode(t, err, "KMSInvalidStateException")

	if _, err := client.CancelKeyDeletion(ctx, &kms.CancelKeyDeletionInput{KeyId: key.KeyId}); err != nil {
		t.Fatal(err)
	}
	if s := state(); s != types.KeyStateDisabled {
		t.Fatalf("state after cancel = %s", s)
	}
	_, err = client.CancelKeyDeletion(ctx, &kms.CancelKeyDeletionInput{KeyId: key.KeyId})
	wantCode(t, err, "KMSInvalidStateException")

	if _, err := client.EnableKey(ctx, &kms.EnableKeyInput{KeyId: key.KeyId}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.EnableKeyRotation(ctx, &kms.EnableKeyRotationInput{KeyId: key.KeyId, RotationPeriodInDays: aws.Int32(180)}); err != nil {
		t.Fatal(err)
	}
	rot, err := client.GetKeyRotationStatus(ctx, &kms.GetKeyRotationStatusInput{KeyId: key.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	if !rot.KeyRotationEnabled || aws.ToInt32(rot.RotationPeriodInDays) != 180 || rot.NextRotationDate == nil {
		t.Fatalf("rotation = %+v", rot)
	}
	if _, err := client.DisableKeyRotation(ctx, &kms.DisableKeyRotationInput{KeyId: key.KeyId}); err != nil {
		t.Fatal(err)
	}
	rot, _ = client.GetKeyRotationStatus(ctx, &kms.GetKeyRotationStatusInput{KeyId: key.KeyId})
	if rot.KeyRotationEnabled {
		t.Fatal("rotation still enabled")
	}

	policy := `{"Version":"2012-10-17","Statement":[]}`
	if _, err := client.PutKeyPolicy(ctx, &kms.PutKeyPolicyInput{KeyId: key.KeyId, Policy: aws.String(policy)}); err != nil {
		t.Fatal(err)
	}
	got, err := client.GetKeyPolicy(ctx, &kms.GetKeyPolicyInput{KeyId: key.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(got.Policy) != policy || aws.ToString(got.PolicyName) != "default" {
		t.Fatalf("policy = %+v", got)
	}
	_, err = client.PutKeyPolicy(ctx, &kms.PutKeyPolicyInput{KeyId: key.KeyId, Policy: aws.String("not json")})
	wantCode(t, err, "MalformedPolicyDocumentException")
	names, err := client.ListKeyPolicies(ctx, &kms.ListKeyPoliciesInput{KeyId: key.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	if len(names.PolicyNames) != 1 || names.PolicyNames[0] != "default" {
		t.Fatalf("policy names = %v", names.PolicyNames)
	}
}

func TestKMSGrants(t *testing.T) {
	ctx := context.Background()
	client := newKMS(t)
	key := createKey(t, client, &kms.CreateKeyInput{})
	grantee := "arn:aws:iam::000000000000:role/app"
	retiree := "arn:aws:iam::000000000000:role/admin"
	g1, err := client.CreateGrant(ctx, &kms.CreateGrantInput{
		KeyId:             key.KeyId,
		GranteePrincipal:  aws.String(grantee),
		RetiringPrincipal: aws.String(retiree),
		Operations:        []types.GrantOperation{types.GrantOperationEncrypt, types.GrantOperationDecrypt},
		Name:              aws.String("app"),
		Constraints:       &types.GrantConstraints{EncryptionContextSubset: map[string]string{"tenant": "t1"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	g2, err := client.CreateGrant(ctx, &kms.CreateGrantInput{KeyId: key.KeyId, GranteePrincipal: aws.String("arn:aws:iam::000000000000:role/other"), Operations: []types.GrantOperation{types.GrantOperationDescribeKey}})
	if err != nil {
		t.Fatal(err)
	}

	list, err := client.ListGrants(ctx, &kms.ListGrantsInput{KeyId: key.KeyId})
	if err != nil {
		t.Fatal(err)
	}
	if len(list.Grants) != 2 {
		t.Fatalf("grants = %+v", list.Grants)
	}
	filtered, _ := client.ListGrants(ctx, &kms.ListGrantsInput{KeyId: key.KeyId, GranteePrincipal: aws.String(grantee)})
	if len(filtered.Grants) != 1 || aws.ToString(filtered.Grants[0].Name) != "app" || filtered.Grants[0].Constraints.EncryptionContextSubset["tenant"] != "t1" {
		t.Fatalf("filtered grants = %+v", filtered.Grants)
	}
	retirable, err := client.ListRetirableGrants(ctx, &kms.ListRetirableGrantsInput{RetiringPrincipal: aws.String(retiree)})
	if err != nil {
		t.Fatal(err)
	}
	if len(retirable.Grants) != 1 || aws.ToString(retirable.Grants[0].GrantId) != aws.ToString(g1.GrantId) {
		t.Fatalf("retirable = %+v", retirable.Grants)
	}

	if _, err := client.RetireGrant(ctx, &kms.RetireGrantInput{GrantToken: g1.GrantToken}); err != nil {
		t.Fatal(err)
	}
	_, err = client.RetireGrant(ctx, &kms.RetireGrantInput{GrantToken: g1.GrantToken})
	wantCode(t, err, "InvalidGrantTokenException")
	if _, err := client.RevokeGrant(ctx, &kms.RevokeGrantInput{KeyId: key.KeyId, GrantId: g2.GrantId}); err != nil {
		t.Fatal(err)
	}
	_, err = client.RevokeGrant(ctx, &kms.RevokeGrantInput{KeyId: key.KeyId, GrantId: g2.GrantId})
	wantCode(t, err, "NotFoundException")
	list, _ = client.ListGrants(ctx, &kms.ListGrantsInput{KeyId: key.KeyId})
	if len(list.Grants) != 0 {
		t.Fatalf("grants after retire and revoke = %+v", list.Grants)
	}
}

func TestKMSMultiRegionReplica(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	east := kms.NewFromConfig(m.Config())
	west := kms.NewFromConfig(m.Config(), func(o *kms.Options) { o.Region = "us-west-2" })
	key := createKey(t, east, &kms.CreateKeyInput{MultiRegion: aws.Bool(true)})
	if !aws.ToBool(key.MultiRegion) || key.MultiRegionConfiguration.MultiRegionKeyType != types.MultiRegionKeyTypePrimary {
		t.Fatalf("metadata = %+v", key)
	}
	if _, err := east.ReplicateKey(ctx, &kms.ReplicateKeyInput{KeyId: key.KeyId, ReplicaRegion: aws.String("us-west-2")}); err != nil {
		t.Fatal(err)
	}
	enc, err := east.Encrypt(ctx, &kms.EncryptInput{KeyId: key.KeyId, Plaintext: []byte("across regions")})
	if err != nil {
		t.Fatal(err)
	}
	dec, err := west.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: enc.CiphertextBlob})
	if err != nil {
		t.Fatal(err)
	}
	if string(dec.Plaintext) != "across regions" {
		t.Fatalf("plaintext = %q", dec.Plaintext)
	}
	desc, _ := east.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: key.KeyId})
	if replicas := desc.KeyMetadata.MultiRegionConfiguration.ReplicaKeys; len(replicas) != 1 || aws.ToString(replicas[0].Region) != "us-west-2" {
		t.Fatalf("replicas = %+v", replicas)
	}
}

func TestKMSMocksAreIsolated(t *testing.T) {
	ctx := context.Background()
	a := newKMS(t)
	b := newKMS(t)
	key := createKey(t, a, &kms.CreateKeyInput{})
	enc, err := a.Encrypt(ctx, &kms.EncryptInput{KeyId: key.KeyId, Plaintext: []byte("mine")})
	if err != nil {
		t.Fatal(err)
	}
	_, err = b.DescribeKey(ctx, &kms.DescribeKeyInput{KeyId: key.KeyId})
	wantCode(t, err, "NotFoundException")
	_, err = b.Decrypt(ctx, &kms.DecryptInput{CiphertextBlob: enc.CiphertextBlob})
	wantCode(t, err, "InvalidCiphertextException")
	keys, err := b.ListKeys(ctx, &kms.ListKeysInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(keys.Keys) != 0 {
		t.Fatalf("keys = %+v", keys.Keys)
	}
}

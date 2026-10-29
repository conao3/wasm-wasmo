import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateAliasCommand,
  CreateKeyCommand,
  DecryptCommand,
  EncryptCommand,
  GenerateDataKeyCommand,
  InvalidCiphertextException,
  KMSClient,
} from "@aws-sdk/client-kms";
import { mockAws } from "../dist/index.js";
import { pattern } from "./helpers.ts";

test("kms encrypt and decrypt with an encryption context", async () => {
  const aws = mockAws();
  const kms = new KMSClient(aws.clientConfig);
  const { KeyMetadata } = await kms.send(new CreateKeyCommand({ Description: "app" }));
  await kms.send(
    new CreateAliasCommand({ AliasName: "alias/app", TargetKeyId: KeyMetadata!.KeyId }),
  );
  const secret = pattern(300);
  const { CiphertextBlob } = await kms.send(
    new EncryptCommand({
      KeyId: "alias/app",
      Plaintext: secret,
      EncryptionContext: { tenant: "a" },
    }),
  );
  assert.notDeepEqual(CiphertextBlob, secret);
  const out = await kms.send(
    new DecryptCommand({ CiphertextBlob, EncryptionContext: { tenant: "a" } }),
  );
  assert.deepEqual(out.Plaintext, secret);
  assert.ok(out.KeyId!.endsWith(KeyMetadata!.KeyId!));
  await assert.rejects(
    kms.send(new DecryptCommand({ CiphertextBlob, EncryptionContext: { tenant: "b" } })),
    InvalidCiphertextException,
  );

  const key = await kms.send(
    new GenerateDataKeyCommand({ KeyId: "alias/app", KeySpec: "AES_256" }),
  );
  assert.equal(key.Plaintext!.length, 32);
  const back = await kms.send(new DecryptCommand({ CiphertextBlob: key.CiphertextBlob }));
  assert.deepEqual(back.Plaintext, key.Plaintext);
  aws.close();
});

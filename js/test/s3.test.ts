import assert from "node:assert/strict";
import { Readable } from "node:stream";
import { describe, test } from "node:test";
import {
  CreateBucketCommand,
  GetObjectCommand,
  HeadObjectCommand,
  ListObjectsV2Command,
  NoSuchKey,
  PutObjectCommand,
  S3Client,
} from "@aws-sdk/client-s3";
import { Upload } from "@aws-sdk/lib-storage";
import { mockAws } from "../dist/index.js";
import { md5, pattern } from "./helpers.ts";

const pathStyleTodo =
  "wasmo-core resolves PUT /<bucket>/ (the JS SDK's path-style bucket URL) as an object operation";

for (const forcePathStyle of [false, true]) {
  const opts = { todo: forcePathStyle && pathStyleTodo };
  describe(forcePathStyle ? "s3 path-style" : "s3 virtual-hosted", () => {
    const setup = async () => {
      const aws = mockAws();
      const s3 = new S3Client({ ...aws.clientConfig, forcePathStyle });
      await s3.send(new CreateBucketCommand({ Bucket: "media" }));
      return { aws, s3 };
    };

    test("binary put and get", opts, async () => {
      const { aws, s3 } = await setup();
      const body = pattern(70_000);
      const put = await s3.send(
        new PutObjectCommand({ Bucket: "media", Key: "a/b.bin", Body: body }),
      );
      assert.equal(put.ETag, `"${md5(body)}"`);
      const got = await s3.send(new GetObjectCommand({ Bucket: "media", Key: "a/b.bin" }));
      assert.ok(got.Body instanceof Readable);
      const bytes = await got.Body.transformToByteArray();
      assert.equal(md5(bytes), md5(body));
      assert.equal(got.ContentLength, body.length);
      const list = await s3.send(new ListObjectsV2Command({ Bucket: "media" }));
      assert.deepEqual(
        list.Contents?.map((o) => o.Key),
        ["a/b.bin"],
      );
      aws.close();
    });

    test("string and stream bodies", opts, async () => {
      const { aws, s3 } = await setup();
      await s3.send(new PutObjectCommand({ Bucket: "media", Key: "string", Body: "hello" }));
      await s3.send(
        new PutObjectCommand({
          Bucket: "media",
          Key: "stream",
          Body: Readable.from([Buffer.from("he"), Buffer.from("llo")]),
          ContentLength: 5,
        }),
      );
      for (const Key of ["string", "stream"]) {
        const got = await s3.send(new GetObjectCommand({ Bucket: "media", Key }));
        assert.equal(await got.Body!.transformToString(), "hello", Key);
      }
      aws.close();
    });

    test("blob and web stream bodies without flexible checksums", opts, async () => {
      const { aws } = await setup();
      const s3 = new S3Client({
        ...aws.clientConfig,
        forcePathStyle,
        requestChecksumCalculation: "WHEN_REQUIRED",
      });
      const web = new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(new TextEncoder().encode("hel"));
          controller.enqueue(new TextEncoder().encode("lo"));
          controller.close();
        },
      });
      const bodies = { blob: new Blob(["hel", "lo"]), web };
      for (const [Key, Body] of Object.entries(bodies)) {
        await s3.send(
          new PutObjectCommand({ Bucket: "media", Key, Body: Body as never, ContentLength: 5 }),
        );
        const got = await s3.send(new GetObjectCommand({ Bucket: "media", Key }));
        assert.equal(await got.Body!.transformToString(), "hello", Key);
      }
      aws.close();
    });

    test("multipart upload through lib-storage", opts, async () => {
      const { aws, s3 } = await setup();
      const body = pattern(12 * 1024 * 1024 + 123);
      const upload = new Upload({
        client: s3,
        params: { Bucket: "media", Key: "big.bin", Body: body },
        partSize: 5 * 1024 * 1024,
        queueSize: 2,
      });
      const out = await upload.done();
      assert.match(out.ETag ?? "", /-3"$/);
      const head = await s3.send(new HeadObjectCommand({ Bucket: "media", Key: "big.bin" }));
      assert.equal(head.ContentLength, body.length);
      const got = await s3.send(new GetObjectCommand({ Bucket: "media", Key: "big.bin" }));
      assert.equal(md5(await got.Body!.transformToByteArray()), md5(body));
      aws.close();
    });

    test("missing key is NoSuchKey", opts, async () => {
      const { aws, s3 } = await setup();
      await assert.rejects(
        s3.send(new GetObjectCommand({ Bucket: "media", Key: "missing" })),
        NoSuchKey,
      );
      aws.close();
    });
  });
}

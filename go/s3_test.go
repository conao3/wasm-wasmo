package wasmo_test

import (
	"bytes"
	"context"
	"crypto/md5"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	awshttp "github.com/aws/aws-sdk-go-v2/aws/transport/http"
	"github.com/aws/aws-sdk-go-v2/feature/s3/manager"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/aws/smithy-go"
	"github.com/conao3/wasm-wasmo/go"
)

var addressing = []struct {
	name      string
	pathStyle bool
}{
	{"virtual-hosted", false},
	{"path-style", true},
}

func s3Client(m *wasmo.Mock, pathStyle bool) *s3.Client {
	return s3.NewFromConfig(m.Config(), func(o *s3.Options) { o.UsePathStyle = pathStyle })
}

func eachAddressing(t *testing.T, f func(t *testing.T, m *wasmo.Mock, c *s3.Client)) {
	for _, a := range addressing {
		t.Run(a.name, func(t *testing.T) {
			m := newMock(t)
			f(t, m, s3Client(m, a.pathStyle))
		})
	}
}

func mustBucket(t *testing.T, c *s3.Client, name string) {
	t.Helper()
	if _, err := c.CreateBucket(context.Background(), &s3.CreateBucketInput{Bucket: aws.String(name)}); err != nil {
		t.Fatal(err)
	}
}

func putString(t *testing.T, c *s3.Client, bucket, key, body string) *s3.PutObjectOutput {
	t.Helper()
	out, err := c.PutObject(context.Background(), &s3.PutObjectInput{
		Bucket: aws.String(bucket), Key: aws.String(key), Body: strings.NewReader(body),
	})
	if err != nil {
		t.Fatal(err)
	}
	return out
}

func readAll(t *testing.T, r io.ReadCloser) []byte {
	t.Helper()
	defer r.Close()
	b, err := io.ReadAll(r)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func pattern(n int) []byte {
	b := make([]byte, n)
	for i := range b {
		b[i] = byte(i*31 + i/7)
	}
	return b
}

func apiCode(err error) string {
	var ae smithy.APIError
	if errors.As(err, &ae) {
		return ae.ErrorCode()
	}
	return ""
}

func httpStatus(err error) int {
	var re *awshttp.ResponseError
	if errors.As(err, &re) {
		return re.HTTPStatusCode()
	}
	return 0
}

func TestS3Buckets(t *testing.T) {
	ctx := context.Background()
	eachAddressing(t, func(t *testing.T, _ *wasmo.Mock, c *s3.Client) {
		mustBucket(t, c, "alpha")
		mustBucket(t, c, "beta")
		if _, err := c.CreateBucket(ctx, &s3.CreateBucketInput{Bucket: aws.String("alpha")}); err != nil {
			t.Fatalf("recreating an owned bucket in us-east-1: %v", err)
		}
		west := func(o *s3.Options) { o.Region = "eu-west-1" }
		if _, err := c.CreateBucket(ctx, &s3.CreateBucketInput{
			Bucket:                    aws.String("gamma"),
			CreateBucketConfiguration: &types.CreateBucketConfiguration{LocationConstraint: types.BucketLocationConstraintEuWest1},
		}, west); err != nil {
			t.Fatal(err)
		}
		if _, err := c.CreateBucket(ctx, &s3.CreateBucketInput{
			Bucket:                    aws.String("gamma"),
			CreateBucketConfiguration: &types.CreateBucketConfiguration{LocationConstraint: types.BucketLocationConstraintEuWest1},
		}, west); apiCode(err) != "BucketAlreadyOwnedByYou" {
			t.Fatalf("recreating an owned bucket outside us-east-1: %v", err)
		}
		if _, err := c.HeadBucket(ctx, &s3.HeadBucketInput{Bucket: aws.String("alpha")}); err != nil {
			t.Fatal(err)
		}
		var nf *types.NotFound
		if _, err := c.HeadBucket(ctx, &s3.HeadBucketInput{Bucket: aws.String("missing")}); !errors.As(err, &nf) {
			t.Fatalf("head missing bucket: %v", err)
		}
		list, err := c.ListBuckets(ctx, &s3.ListBucketsInput{})
		if err != nil {
			t.Fatal(err)
		}
		if len(list.Buckets) != 3 || aws.ToString(list.Buckets[0].Name) != "alpha" {
			t.Fatalf("buckets = %+v", list.Buckets)
		}
		loc, err := c.GetBucketLocation(ctx, &s3.GetBucketLocationInput{Bucket: aws.String("alpha")})
		if err != nil || loc.LocationConstraint != "" {
			t.Fatalf("location = %+v, %v", loc, err)
		}
		putString(t, c, "alpha", "k", "v")
		if _, err := c.DeleteBucket(ctx, &s3.DeleteBucketInput{Bucket: aws.String("alpha")}); apiCode(err) != "BucketNotEmpty" || httpStatus(err) != 409 {
			t.Fatalf("delete non-empty: %v", err)
		}
		if _, err := c.DeleteBucket(ctx, &s3.DeleteBucketInput{Bucket: aws.String("beta")}); err != nil {
			t.Fatal(err)
		}
		if _, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("beta"), Key: aws.String("k")}); apiCode(err) != "NoSuchBucket" || httpStatus(err) != 404 {
			t.Fatalf("get from deleted bucket: %v", err)
		}
	})
}

func TestS3BucketInRegion(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := s3.NewFromConfig(m.Config(), func(o *s3.Options) { o.Region = "eu-west-1" })
	if _, err := c.CreateBucket(ctx, &s3.CreateBucketInput{Bucket: aws.String("eu-bucket")}); apiCode(err) != "IllegalLocationConstraintException" {
		t.Fatalf("create without constraint: %v", err)
	}
	if _, err := c.CreateBucket(ctx, &s3.CreateBucketInput{
		Bucket:                    aws.String("eu-bucket"),
		CreateBucketConfiguration: &types.CreateBucketConfiguration{LocationConstraint: types.BucketLocationConstraintEuWest1},
	}); err != nil {
		t.Fatal(err)
	}
	loc, err := c.GetBucketLocation(ctx, &s3.GetBucketLocationInput{Bucket: aws.String("eu-bucket")})
	if err != nil || loc.LocationConstraint != types.BucketLocationConstraintEuWest1 {
		t.Fatalf("location = %+v, %v", loc, err)
	}
	head, err := c.HeadBucket(ctx, &s3.HeadBucketInput{Bucket: aws.String("eu-bucket")})
	if err != nil || aws.ToString(head.BucketRegion) != "eu-west-1" {
		t.Fatalf("head = %+v, %v", head, err)
	}
}

func TestS3ObjectRoundTrip(t *testing.T) {
	ctx := context.Background()
	eachAddressing(t, func(t *testing.T, _ *wasmo.Mock, c *s3.Client) {
		mustBucket(t, c, "data")
		binary := make([]byte, 256)
		for i := range binary {
			binary[i] = byte(i)
		}
		big := pattern(5*1024*1024 + 123)
		keys := map[string][]byte{
			"bin/all-bytes":             binary,
			"big/object.bin":            big,
			"with space/and+plus.txt":   []byte("spaces"),
			"日本語/ファイル名.txt":             []byte("unicode"),
			"deep/a/b/c/d/":             {},
			"special/!$&'()*,;=@:%.txt": []byte("reserved"),
		}
		for key, body := range keys {
			put, err := c.PutObject(ctx, &s3.PutObjectInput{
				Bucket:      aws.String("data"),
				Key:         aws.String(key),
				Body:        bytes.NewReader(body),
				ContentType: aws.String("application/x-test"),
				Metadata:    map[string]string{"owner": "wasmo", "mixed-case": "Value 値"},
			})
			if err != nil {
				t.Fatalf("put %q: %v", key, err)
			}
			sum := md5.Sum(body)
			if want := `"` + hex.EncodeToString(sum[:]) + `"`; aws.ToString(put.ETag) != want {
				t.Fatalf("etag %q = %s, want %s", key, aws.ToString(put.ETag), want)
			}
		}
		for key, body := range keys {
			got, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("data"), Key: aws.String(key)})
			if err != nil {
				t.Fatalf("get %q: %v", key, err)
			}
			if b := readAll(t, got.Body); !bytes.Equal(b, body) {
				t.Fatalf("body %q: %d bytes, want %d", key, len(b), len(body))
			}
			if aws.ToString(got.ContentType) != "application/x-test" || aws.ToInt64(got.ContentLength) != int64(len(body)) {
				t.Fatalf("headers %q: %s %d", key, aws.ToString(got.ContentType), aws.ToInt64(got.ContentLength))
			}
			if got.Metadata["owner"] != "wasmo" || got.Metadata["mixed-case"] != "Value 値" {
				t.Fatalf("metadata %q = %v", key, got.Metadata)
			}
			if got.LastModified == nil || time.Since(*got.LastModified) > time.Minute {
				t.Fatalf("last modified = %v", got.LastModified)
			}
		}
		head, err := c.HeadObject(ctx, &s3.HeadObjectInput{Bucket: aws.String("data"), Key: aws.String("big/object.bin")})
		if err != nil || aws.ToInt64(head.ContentLength) != int64(len(big)) || head.Metadata["owner"] != "wasmo" {
			t.Fatalf("head = %+v, %v", head, err)
		}
		var nsk *types.NoSuchKey
		if _, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("data"), Key: aws.String("nope")}); !errors.As(err, &nsk) || httpStatus(err) != 404 {
			t.Fatalf("missing key: %v", err)
		}
		var nf *types.NotFound
		if _, err := c.HeadObject(ctx, &s3.HeadObjectInput{Bucket: aws.String("data"), Key: aws.String("nope")}); !errors.As(err, &nf) {
			t.Fatalf("head missing key: %v", err)
		}
		if _, err := c.DeleteObject(ctx, &s3.DeleteObjectInput{Bucket: aws.String("data"), Key: aws.String("bin/all-bytes")}); err != nil {
			t.Fatal(err)
		}
		if _, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("data"), Key: aws.String("bin/all-bytes")}); !errors.As(err, &nsk) {
			t.Fatalf("deleted key: %v", err)
		}
	})
}

func TestS3RangeAndConditionalGet(t *testing.T) {
	ctx := context.Background()
	eachAddressing(t, func(t *testing.T, _ *wasmo.Mock, c *s3.Client) {
		mustBucket(t, c, "ranges")
		put := putString(t, c, "ranges", "digits", "0123456789")
		cases := map[string]string{"bytes=2-5": "2345", "bytes=7-": "789", "bytes=-3": "789", "bytes=8-100": "89"}
		for rng, want := range cases {
			got, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ranges"), Key: aws.String("digits"), Range: aws.String(rng)})
			if err != nil {
				t.Fatalf("%s: %v", rng, err)
			}
			if b := string(readAll(t, got.Body)); b != want {
				t.Fatalf("%s = %q, want %q", rng, b, want)
			}
			if !strings.HasPrefix(aws.ToString(got.ContentRange), "bytes ") || !strings.HasSuffix(aws.ToString(got.ContentRange), "/10") {
				t.Fatalf("%s content-range = %q", rng, aws.ToString(got.ContentRange))
			}
		}
		if _, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ranges"), Key: aws.String("digits"), Range: aws.String("bytes=20-30")}); apiCode(err) != "InvalidRange" || httpStatus(err) != 416 {
			t.Fatalf("unsatisfiable range: %v", err)
		}

		_, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ranges"), Key: aws.String("digits"), IfNoneMatch: put.ETag})
		if httpStatus(err) != 304 || apiCode(err) != "NotModified" {
			t.Fatalf("if-none-match: %v", err)
		}
		_, err = c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ranges"), Key: aws.String("digits"), IfMatch: aws.String(`"0000"`)})
		if httpStatus(err) != 412 || apiCode(err) != "PreconditionFailed" {
			t.Fatalf("if-match: %v", err)
		}
		future := time.Now().Add(time.Hour)
		if _, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ranges"), Key: aws.String("digits"), IfModifiedSince: &future}); httpStatus(err) != 304 {
			t.Fatalf("if-modified-since: %v", err)
		}
		past := time.Now().Add(-time.Hour)
		if _, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ranges"), Key: aws.String("digits"), IfUnmodifiedSince: &past}); httpStatus(err) != 412 {
			t.Fatalf("if-unmodified-since: %v", err)
		}
		if _, err := c.HeadObject(ctx, &s3.HeadObjectInput{Bucket: aws.String("ranges"), Key: aws.String("digits"), IfNoneMatch: put.ETag}); httpStatus(err) != 304 {
			t.Fatalf("head if-none-match: %v", err)
		}
		got, err := c.GetObject(ctx, &s3.GetObjectInput{
			Bucket: aws.String("ranges"), Key: aws.String("digits"), IfMatch: put.ETag,
			ResponseContentType:        aws.String("text/plain"),
			ResponseContentDisposition: aws.String(`attachment; filename="d.txt"`),
			ResponseCacheControl:       aws.String("no-cache"),
		})
		if err != nil {
			t.Fatal(err)
		}
		readAll(t, got.Body)
		if aws.ToString(got.ContentType) != "text/plain" || aws.ToString(got.ContentDisposition) != `attachment; filename="d.txt"` || aws.ToString(got.CacheControl) != "no-cache" {
			t.Fatalf("overrides = %s / %s / %s", aws.ToString(got.ContentType), aws.ToString(got.ContentDisposition), aws.ToString(got.CacheControl))
		}
		if _, err := c.PutObject(ctx, &s3.PutObjectInput{Bucket: aws.String("ranges"), Key: aws.String("digits"), Body: strings.NewReader("x"), IfNoneMatch: aws.String("*")}); httpStatus(err) != 412 {
			t.Fatalf("put if-none-match: %v", err)
		}
	})
}

func TestS3ListObjectsV2(t *testing.T) {
	ctx := context.Background()
	eachAddressing(t, func(t *testing.T, _ *wasmo.Mock, c *s3.Client) {
		mustBucket(t, c, "list")
		var keys []string
		for i := range 7 {
			keys = append(keys, fmt.Sprintf("logs/2026/%02d.log", i))
		}
		keys = append(keys, "logs/readme", "photos/a.jpg", "photos/b.jpg", "root.txt", "space dir/x")
		for _, k := range keys {
			putString(t, c, "list", k, k)
		}

		var all []string
		p := s3.NewListObjectsV2Paginator(c, &s3.ListObjectsV2Input{Bucket: aws.String("list"), MaxKeys: aws.Int32(3)})
		pages := 0
		for p.HasMorePages() {
			out, err := p.NextPage(ctx)
			if err != nil {
				t.Fatal(err)
			}
			pages++
			for _, o := range out.Contents {
				all = append(all, aws.ToString(o.Key))
			}
		}
		sorted := append([]string(nil), keys...)
		sort.Strings(sorted)
		if strings.Join(all, ",") != strings.Join(sorted, ",") || pages != 4 {
			t.Fatalf("paginated %d pages: %v", pages, all)
		}

		var objects, prefixes []string
		p = s3.NewListObjectsV2Paginator(c, &s3.ListObjectsV2Input{Bucket: aws.String("list"), Delimiter: aws.String("/"), MaxKeys: aws.Int32(1)})
		for p.HasMorePages() {
			out, err := p.NextPage(ctx)
			if err != nil {
				t.Fatal(err)
			}
			for _, o := range out.Contents {
				objects = append(objects, aws.ToString(o.Key))
			}
			for _, cp := range out.CommonPrefixes {
				prefixes = append(prefixes, aws.ToString(cp.Prefix))
			}
		}
		if strings.Join(objects, ",") != "root.txt" || strings.Join(prefixes, ",") != "logs/,photos/,space dir/" {
			t.Fatalf("delimited: objects=%v prefixes=%v", objects, prefixes)
		}

		out, err := c.ListObjectsV2(ctx, &s3.ListObjectsV2Input{Bucket: aws.String("list"), Prefix: aws.String("logs/"), Delimiter: aws.String("/"), StartAfter: aws.String("logs/2026/")})
		if err != nil {
			t.Fatal(err)
		}
		if len(out.Contents) != 1 || aws.ToString(out.Contents[0].Key) != "logs/readme" || len(out.CommonPrefixes) != 0 {
			t.Fatalf("start-after: %+v %+v", out.Contents, out.CommonPrefixes)
		}

		enc, err := c.ListObjectsV2(ctx, &s3.ListObjectsV2Input{Bucket: aws.String("list"), Prefix: aws.String("space "), EncodingType: types.EncodingTypeUrl})
		if err != nil {
			t.Fatal(err)
		}
		if len(enc.Contents) != 1 || aws.ToString(enc.Contents[0].Key) != "space%20dir/x" {
			t.Fatalf("url encoded: %+v", enc.Contents)
		}

		v1, err := c.ListObjects(ctx, &s3.ListObjectsInput{Bucket: aws.String("list"), Prefix: aws.String("photos/"), MaxKeys: aws.Int32(1)})
		if err != nil {
			t.Fatal(err)
		}
		if len(v1.Contents) != 1 || !aws.ToBool(v1.IsTruncated) || aws.ToString(v1.NextMarker) != "photos/a.jpg" {
			t.Fatalf("list v1: %+v", v1)
		}
		v1, err = c.ListObjects(ctx, &s3.ListObjectsInput{Bucket: aws.String("list"), Prefix: aws.String("photos/"), Marker: v1.NextMarker})
		if err != nil || len(v1.Contents) != 1 || aws.ToString(v1.Contents[0].Key) != "photos/b.jpg" {
			t.Fatalf("list v1 marker: %+v %v", v1, err)
		}
	})
}

func TestS3CopyAndDeleteObjects(t *testing.T) {
	ctx := context.Background()
	eachAddressing(t, func(t *testing.T, _ *wasmo.Mock, c *s3.Client) {
		mustBucket(t, c, "src")
		mustBucket(t, c, "dst")
		_, err := c.PutObject(ctx, &s3.PutObjectInput{
			Bucket: aws.String("src"), Key: aws.String("dir/orig name.txt"), Body: strings.NewReader("copy me"),
			ContentType: aws.String("text/plain"), Metadata: map[string]string{"a": "1"},
		})
		if err != nil {
			t.Fatal(err)
		}
		copied, err := c.CopyObject(ctx, &s3.CopyObjectInput{
			Bucket: aws.String("dst"), Key: aws.String("copy.txt"), CopySource: aws.String("src/dir/orig%20name.txt"),
		})
		if err != nil {
			t.Fatal(err)
		}
		got, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("dst"), Key: aws.String("copy.txt")})
		if err != nil {
			t.Fatal(err)
		}
		if string(readAll(t, got.Body)) != "copy me" || got.Metadata["a"] != "1" || aws.ToString(got.ContentType) != "text/plain" || aws.ToString(got.ETag) != aws.ToString(copied.CopyObjectResult.ETag) {
			t.Fatalf("copy = %+v", got)
		}
		if _, err := c.CopyObject(ctx, &s3.CopyObjectInput{
			Bucket: aws.String("dst"), Key: aws.String("copy.txt"), CopySource: aws.String("dst/copy.txt"),
			MetadataDirective: types.MetadataDirectiveReplace, Metadata: map[string]string{"b": "2"}, ContentType: aws.String("text/markdown"),
		}); err != nil {
			t.Fatal(err)
		}
		head, err := c.HeadObject(ctx, &s3.HeadObjectInput{Bucket: aws.String("dst"), Key: aws.String("copy.txt")})
		if err != nil || head.Metadata["b"] != "2" || head.Metadata["a"] != "" || aws.ToString(head.ContentType) != "text/markdown" {
			t.Fatalf("replace = %+v, %v", head, err)
		}
		if _, err := c.CopyObject(ctx, &s3.CopyObjectInput{Bucket: aws.String("dst"), Key: aws.String("copy.txt"), CopySource: aws.String("dst/copy.txt")}); apiCode(err) != "InvalidRequest" {
			t.Fatalf("self copy: %v", err)
		}
		if _, err := c.CopyObject(ctx, &s3.CopyObjectInput{Bucket: aws.String("dst"), Key: aws.String("x"), CopySource: aws.String("src/missing")}); apiCode(err) != "NoSuchKey" {
			t.Fatalf("copy missing: %v", err)
		}

		for _, k := range []string{"a", "b", "c"} {
			putString(t, c, "dst", k, k)
		}
		del, err := c.DeleteObjects(ctx, &s3.DeleteObjectsInput{Bucket: aws.String("dst"), Delete: &types.Delete{Objects: []types.ObjectIdentifier{
			{Key: aws.String("a")}, {Key: aws.String("c")}, {Key: aws.String("never-existed")},
		}}})
		if err != nil {
			t.Fatal(err)
		}
		if len(del.Deleted) != 3 || len(del.Errors) != 0 {
			t.Fatalf("delete objects = %+v", del)
		}
		list, err := c.ListObjectsV2(ctx, &s3.ListObjectsV2Input{Bucket: aws.String("dst")})
		if err != nil {
			t.Fatal(err)
		}
		var left []string
		for _, o := range list.Contents {
			left = append(left, aws.ToString(o.Key))
		}
		if strings.Join(left, ",") != "b,copy.txt" {
			t.Fatalf("remaining = %v", left)
		}
	})
}

func TestS3MultipartManager(t *testing.T) {
	ctx := context.Background()
	eachAddressing(t, func(t *testing.T, _ *wasmo.Mock, c *s3.Client) {
		mustBucket(t, c, "multipart")
		body := pattern(12*1024*1024 + 777)
		up := manager.NewUploader(c, func(u *manager.Uploader) { u.PartSize = 5 * 1024 * 1024 })
		res, err := up.Upload(ctx, &s3.PutObjectInput{Bucket: aws.String("multipart"), Key: aws.String("large/file.bin"), Body: bytes.NewReader(body), ContentType: aws.String("application/x-big")})
		if err != nil {
			t.Fatal(err)
		}
		if !strings.HasSuffix(aws.ToString(res.ETag), `-3"`) || res.UploadID == "" {
			t.Fatalf("upload result: etag=%s id=%s", aws.ToString(res.ETag), res.UploadID)
		}
		head, err := c.HeadObject(ctx, &s3.HeadObjectInput{Bucket: aws.String("multipart"), Key: aws.String("large/file.bin")})
		if err != nil || aws.ToInt64(head.ContentLength) != int64(len(body)) || aws.ToString(head.ContentType) != "application/x-big" {
			t.Fatalf("head = %+v, %v", head, err)
		}

		dl := manager.NewDownloader(c, func(d *manager.Downloader) { d.PartSize = 4 * 1024 * 1024 })
		buf := manager.NewWriteAtBuffer(nil)
		n, err := dl.Download(ctx, buf, &s3.GetObjectInput{Bucket: aws.String("multipart"), Key: aws.String("large/file.bin")})
		if err != nil {
			t.Fatal(err)
		}
		if n != int64(len(body)) || !bytes.Equal(buf.Bytes(), body) {
			t.Fatalf("downloaded %d bytes, equal=%v", n, bytes.Equal(buf.Bytes(), body))
		}

		created, err := c.CreateMultipartUpload(ctx, &s3.CreateMultipartUploadInput{Bucket: aws.String("multipart"), Key: aws.String("pending")})
		if err != nil {
			t.Fatal(err)
		}
		if _, err := c.UploadPart(ctx, &s3.UploadPartInput{Bucket: aws.String("multipart"), Key: aws.String("pending"), UploadId: created.UploadId, PartNumber: aws.Int32(1), Body: strings.NewReader("part one")}); err != nil {
			t.Fatal(err)
		}
		if _, err := c.UploadPartCopy(ctx, &s3.UploadPartCopyInput{Bucket: aws.String("multipart"), Key: aws.String("pending"), UploadId: created.UploadId, PartNumber: aws.Int32(2), CopySource: aws.String("multipart/large/file.bin"), CopySourceRange: aws.String("bytes=0-9")}); err != nil {
			t.Fatal(err)
		}
		parts, err := c.ListParts(ctx, &s3.ListPartsInput{Bucket: aws.String("multipart"), Key: aws.String("pending"), UploadId: created.UploadId})
		if err != nil || len(parts.Parts) != 2 || aws.ToInt64(parts.Parts[1].Size) != 10 {
			t.Fatalf("parts = %+v, %v", parts, err)
		}
		uploads, err := c.ListMultipartUploads(ctx, &s3.ListMultipartUploadsInput{Bucket: aws.String("multipart")})
		if err != nil || len(uploads.Uploads) != 1 || aws.ToString(uploads.Uploads[0].Key) != "pending" {
			t.Fatalf("uploads = %+v, %v", uploads, err)
		}
		if _, err := c.CompleteMultipartUpload(ctx, &s3.CompleteMultipartUploadInput{
			Bucket: aws.String("multipart"), Key: aws.String("pending"), UploadId: created.UploadId,
			MultipartUpload: &types.CompletedMultipartUpload{Parts: []types.CompletedPart{
				{PartNumber: aws.Int32(1), ETag: parts.Parts[0].ETag}, {PartNumber: aws.Int32(2), ETag: parts.Parts[1].ETag},
			}},
		}); apiCode(err) != "EntityTooSmall" {
			t.Fatalf("complete with small part: %v", err)
		}
		if _, err := c.AbortMultipartUpload(ctx, &s3.AbortMultipartUploadInput{Bucket: aws.String("multipart"), Key: aws.String("pending"), UploadId: created.UploadId}); err != nil {
			t.Fatal(err)
		}
		if _, err := c.ListParts(ctx, &s3.ListPartsInput{Bucket: aws.String("multipart"), Key: aws.String("pending"), UploadId: created.UploadId}); apiCode(err) != "NoSuchUpload" || httpStatus(err) != 404 {
			t.Fatalf("aborted upload: %v", err)
		}
	})
}

func TestS3MultipartETag(t *testing.T) {
	ctx := context.Background()
	c := s3Client(newMock(t), false)
	mustBucket(t, c, "etag")
	created, err := c.CreateMultipartUpload(ctx, &s3.CreateMultipartUploadInput{Bucket: aws.String("etag"), Key: aws.String("k")})
	if err != nil {
		t.Fatal(err)
	}
	p1 := pattern(5 * 1024 * 1024)
	p2 := []byte("tail")
	var completed []types.CompletedPart
	var md5s []byte
	for i, p := range [][]byte{p1, p2} {
		out, err := c.UploadPart(ctx, &s3.UploadPartInput{Bucket: aws.String("etag"), Key: aws.String("k"), UploadId: created.UploadId, PartNumber: aws.Int32(int32(i + 1)), Body: bytes.NewReader(p)})
		if err != nil {
			t.Fatal(err)
		}
		completed = append(completed, types.CompletedPart{PartNumber: aws.Int32(int32(i + 1)), ETag: out.ETag})
		sum := md5.Sum(p)
		md5s = append(md5s, sum[:]...)
	}
	done, err := c.CompleteMultipartUpload(ctx, &s3.CompleteMultipartUploadInput{Bucket: aws.String("etag"), Key: aws.String("k"), UploadId: created.UploadId, MultipartUpload: &types.CompletedMultipartUpload{Parts: completed}})
	if err != nil {
		t.Fatal(err)
	}
	sum := md5.Sum(md5s)
	if want := `"` + hex.EncodeToString(sum[:]) + `-2"`; aws.ToString(done.ETag) != want {
		t.Fatalf("etag = %s, want %s", aws.ToString(done.ETag), want)
	}
	got, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("etag"), Key: aws.String("k"), PartNumber: aws.Int32(2)})
	if err != nil {
		t.Fatal(err)
	}
	if string(readAll(t, got.Body)) != "tail" || aws.ToInt32(got.PartsCount) != 2 {
		t.Fatalf("part 2: parts=%d", aws.ToInt32(got.PartsCount))
	}
}

func TestS3Versioning(t *testing.T) {
	ctx := context.Background()
	eachAddressing(t, func(t *testing.T, _ *wasmo.Mock, c *s3.Client) {
		mustBucket(t, c, "ver")
		if _, err := c.PutBucketVersioning(ctx, &s3.PutBucketVersioningInput{Bucket: aws.String("ver"), VersioningConfiguration: &types.VersioningConfiguration{Status: types.BucketVersioningStatusEnabled}}); err != nil {
			t.Fatal(err)
		}
		status, err := c.GetBucketVersioning(ctx, &s3.GetBucketVersioningInput{Bucket: aws.String("ver")})
		if err != nil || status.Status != types.BucketVersioningStatusEnabled {
			t.Fatalf("versioning = %+v, %v", status, err)
		}
		v1 := putString(t, c, "ver", "doc", "first")
		v2 := putString(t, c, "ver", "doc", "second")
		if aws.ToString(v1.VersionId) == "" || aws.ToString(v1.VersionId) == aws.ToString(v2.VersionId) {
			t.Fatalf("version ids %q %q", aws.ToString(v1.VersionId), aws.ToString(v2.VersionId))
		}
		old, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ver"), Key: aws.String("doc"), VersionId: v1.VersionId})
		if err != nil || string(readAll(t, old.Body)) != "first" {
			t.Fatalf("old version: %v", err)
		}
		del, err := c.DeleteObject(ctx, &s3.DeleteObjectInput{Bucket: aws.String("ver"), Key: aws.String("doc")})
		if err != nil || !aws.ToBool(del.DeleteMarker) || aws.ToString(del.VersionId) == "" {
			t.Fatalf("delete = %+v, %v", del, err)
		}
		var nsk *types.NoSuchKey
		if _, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ver"), Key: aws.String("doc")}); !errors.As(err, &nsk) {
			t.Fatalf("get behind delete marker: %v", err)
		}
		versions, err := c.ListObjectVersions(ctx, &s3.ListObjectVersionsInput{Bucket: aws.String("ver")})
		if err != nil {
			t.Fatal(err)
		}
		if len(versions.Versions) != 2 || len(versions.DeleteMarkers) != 1 || !aws.ToBool(versions.DeleteMarkers[0].IsLatest) || aws.ToBool(versions.Versions[0].IsLatest) {
			t.Fatalf("versions = %+v markers = %+v", versions.Versions, versions.DeleteMarkers)
		}
		if aws.ToString(versions.Versions[0].VersionId) != aws.ToString(v2.VersionId) {
			t.Fatalf("newest version first: %s", aws.ToString(versions.Versions[0].VersionId))
		}
		if _, err := c.DeleteObject(ctx, &s3.DeleteObjectInput{Bucket: aws.String("ver"), Key: aws.String("doc"), VersionId: del.VersionId}); err != nil {
			t.Fatal(err)
		}
		restored, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("ver"), Key: aws.String("doc")})
		if err != nil || string(readAll(t, restored.Body)) != "second" {
			t.Fatalf("restored: %v", err)
		}
	})
}

func TestS3TaggingAndConfiguration(t *testing.T) {
	ctx := context.Background()
	eachAddressing(t, func(t *testing.T, _ *wasmo.Mock, c *s3.Client) {
		mustBucket(t, c, "cfg")
		if _, err := c.PutObject(ctx, &s3.PutObjectInput{Bucket: aws.String("cfg"), Key: aws.String("t"), Body: strings.NewReader("x"), Tagging: aws.String("env=dev&team=a%20b")}); err != nil {
			t.Fatal(err)
		}
		tags, err := c.GetObjectTagging(ctx, &s3.GetObjectTaggingInput{Bucket: aws.String("cfg"), Key: aws.String("t")})
		if err != nil || len(tags.TagSet) != 2 || aws.ToString(tags.TagSet[1].Value) != "a b" {
			t.Fatalf("object tags = %+v, %v", tags, err)
		}
		if _, err := c.PutObjectTagging(ctx, &s3.PutObjectTaggingInput{Bucket: aws.String("cfg"), Key: aws.String("t"), Tagging: &types.Tagging{TagSet: []types.Tag{{Key: aws.String("k"), Value: aws.String("v")}}}}); err != nil {
			t.Fatal(err)
		}
		if _, err := c.GetBucketTagging(ctx, &s3.GetBucketTaggingInput{Bucket: aws.String("cfg")}); apiCode(err) != "NoSuchTagSet" {
			t.Fatalf("missing bucket tags: %v", err)
		}
		if _, err := c.PutBucketTagging(ctx, &s3.PutBucketTaggingInput{Bucket: aws.String("cfg"), Tagging: &types.Tagging{TagSet: []types.Tag{{Key: aws.String("project"), Value: aws.String("wasmo")}}}}); err != nil {
			t.Fatal(err)
		}
		bt, err := c.GetBucketTagging(ctx, &s3.GetBucketTaggingInput{Bucket: aws.String("cfg")})
		if err != nil || len(bt.TagSet) != 1 || aws.ToString(bt.TagSet[0].Value) != "wasmo" {
			t.Fatalf("bucket tags = %+v, %v", bt, err)
		}

		policy := `{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"arn:aws:s3:::cfg/*"}]}`
		if _, err := c.PutBucketPolicy(ctx, &s3.PutBucketPolicyInput{Bucket: aws.String("cfg"), Policy: aws.String(policy)}); err != nil {
			t.Fatal(err)
		}
		gp, err := c.GetBucketPolicy(ctx, &s3.GetBucketPolicyInput{Bucket: aws.String("cfg")})
		if err != nil || aws.ToString(gp.Policy) != policy {
			t.Fatalf("policy = %v, %v", gp, err)
		}

		if _, err := c.GetBucketCors(ctx, &s3.GetBucketCorsInput{Bucket: aws.String("cfg")}); apiCode(err) != "NoSuchCORSConfiguration" {
			t.Fatalf("missing cors: %v", err)
		}
		if _, err := c.PutBucketCors(ctx, &s3.PutBucketCorsInput{Bucket: aws.String("cfg"), CORSConfiguration: &types.CORSConfiguration{CORSRules: []types.CORSRule{{
			AllowedMethods: []string{"GET", "PUT"}, AllowedOrigins: []string{"*"}, MaxAgeSeconds: aws.Int32(300),
		}}}}); err != nil {
			t.Fatal(err)
		}
		cors, err := c.GetBucketCors(ctx, &s3.GetBucketCorsInput{Bucket: aws.String("cfg")})
		if err != nil || len(cors.CORSRules) != 1 || len(cors.CORSRules[0].AllowedMethods) != 2 || aws.ToInt32(cors.CORSRules[0].MaxAgeSeconds) != 300 {
			t.Fatalf("cors = %+v, %v", cors, err)
		}

		if _, err := c.PutBucketLifecycleConfiguration(ctx, &s3.PutBucketLifecycleConfigurationInput{Bucket: aws.String("cfg"), LifecycleConfiguration: &types.BucketLifecycleConfiguration{Rules: []types.LifecycleRule{{
			ID: aws.String("expire"), Status: types.ExpirationStatusEnabled, Filter: &types.LifecycleRuleFilter{Prefix: aws.String("tmp/")}, Expiration: &types.LifecycleExpiration{Days: aws.Int32(7)},
		}}}}); err != nil {
			t.Fatal(err)
		}
		lc, err := c.GetBucketLifecycleConfiguration(ctx, &s3.GetBucketLifecycleConfigurationInput{Bucket: aws.String("cfg")})
		if err != nil || len(lc.Rules) != 1 || aws.ToInt32(lc.Rules[0].Expiration.Days) != 7 {
			t.Fatalf("lifecycle = %+v, %v", lc, err)
		}

		enc, err := c.GetBucketEncryption(ctx, &s3.GetBucketEncryptionInput{Bucket: aws.String("cfg")})
		if err != nil || enc.ServerSideEncryptionConfiguration.Rules[0].ApplyServerSideEncryptionByDefault.SSEAlgorithm != types.ServerSideEncryptionAes256 {
			t.Fatalf("default encryption = %+v, %v", enc, err)
		}
		if _, err := c.PutBucketWebsite(ctx, &s3.PutBucketWebsiteInput{Bucket: aws.String("cfg"), WebsiteConfiguration: &types.WebsiteConfiguration{IndexDocument: &types.IndexDocument{Suffix: aws.String("index.html")}}}); err != nil {
			t.Fatal(err)
		}
		web, err := c.GetBucketWebsite(ctx, &s3.GetBucketWebsiteInput{Bucket: aws.String("cfg")})
		if err != nil || aws.ToString(web.IndexDocument.Suffix) != "index.html" {
			t.Fatalf("website = %+v, %v", web, err)
		}
		if _, err := c.PutBucketNotificationConfiguration(ctx, &s3.PutBucketNotificationConfigurationInput{Bucket: aws.String("cfg"), NotificationConfiguration: &types.NotificationConfiguration{
			QueueConfigurations: []types.QueueConfiguration{{QueueArn: aws.String("arn:aws:sqs:us-east-1:000000000000:q"), Events: []types.Event{types.EventS3ObjectCreated}}},
		}}); err != nil {
			t.Fatal(err)
		}
		notif, err := c.GetBucketNotificationConfiguration(ctx, &s3.GetBucketNotificationConfigurationInput{Bucket: aws.String("cfg")})
		if err != nil || len(notif.QueueConfigurations) != 1 || notif.QueueConfigurations[0].Events[0] != types.EventS3ObjectCreated {
			t.Fatalf("notification = %+v, %v", notif, err)
		}

		acl, err := c.GetBucketAcl(ctx, &s3.GetBucketAclInput{Bucket: aws.String("cfg")})
		if err != nil || len(acl.Grants) != 1 || acl.Grants[0].Permission != types.PermissionFullControl || acl.Grants[0].Grantee.Type != types.TypeCanonicalUser {
			t.Fatalf("bucket acl = %+v, %v", acl, err)
		}
		if _, err := c.PutObjectAcl(ctx, &s3.PutObjectAclInput{Bucket: aws.String("cfg"), Key: aws.String("t"), ACL: types.ObjectCannedACLPublicRead}); err != nil {
			t.Fatal(err)
		}
		oacl, err := c.GetObjectAcl(ctx, &s3.GetObjectAclInput{Bucket: aws.String("cfg"), Key: aws.String("t")})
		if err != nil || len(oacl.Grants) != 2 {
			t.Fatalf("object acl = %+v, %v", oacl, err)
		}
	})
}

func TestS3PresignedGet(t *testing.T) {
	ctx := context.Background()
	for _, a := range addressing {
		t.Run(a.name, func(t *testing.T) {
			m := newMock(t)
			c := s3Client(m, a.pathStyle)
			mustBucket(t, c, "share")
			putString(t, c, "share", "docs/hello world.txt", "presigned body")
			req, err := s3.NewPresignClient(c).PresignGetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("share"), Key: aws.String("docs/hello world.txt")}, s3.WithPresignExpires(5*time.Minute))
			if err != nil {
				t.Fatal(err)
			}
			if !strings.Contains(req.URL, "X-Amz-Signature=") {
				t.Fatalf("url = %s", req.URL)
			}
			httpReq, err := http.NewRequestWithContext(ctx, req.Method, req.URL, nil)
			if err != nil {
				t.Fatal(err)
			}
			res, err := m.Do(httpReq)
			if err != nil {
				t.Fatal(err)
			}
			if b := readAll(t, res.Body); res.StatusCode != 200 || string(b) != "presigned body" {
				t.Fatalf("presigned GET: %d %q", res.StatusCode, b)
			}
		})
	}
}

func TestS3MocksAreIsolated(t *testing.T) {
	ctx := context.Background()
	a := s3Client(newMock(t), false)
	b := s3Client(newMock(t), false)
	mustBucket(t, a, "shared-name")
	putString(t, a, "shared-name", "k", "from a")
	if _, err := b.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("shared-name"), Key: aws.String("k")}); apiCode(err) != "NoSuchBucket" {
		t.Fatalf("mock b sees mock a: %v", err)
	}
	mustBucket(t, b, "shared-name")
	list, err := b.ListObjectsV2(ctx, &s3.ListObjectsV2Input{Bucket: aws.String("shared-name")})
	if err != nil || len(list.Contents) != 0 {
		t.Fatalf("mock b objects = %+v, %v", list, err)
	}
}

func BenchmarkS3(b *testing.B) {
	ctx := context.Background()
	m, err := wasmo.New(ctx)
	if err != nil {
		b.Fatal(err)
	}
	defer m.Close(ctx)
	c := s3Client(m, false)
	if _, err := c.CreateBucket(ctx, &s3.CreateBucketInput{Bucket: aws.String("bench")}); err != nil {
		b.Fatal(err)
	}
	for _, size := range []int{1024, 1024 * 1024} {
		body := pattern(size)
		key := aws.String(fmt.Sprintf("obj-%d", size))
		b.Run(fmt.Sprintf("PutObject/%dKiB", size/1024), func(b *testing.B) {
			for b.Loop() {
				if _, err := c.PutObject(ctx, &s3.PutObjectInput{Bucket: aws.String("bench"), Key: key, Body: bytes.NewReader(body)}); err != nil {
					b.Fatal(err)
				}
			}
		})
		b.Run(fmt.Sprintf("GetObject/%dKiB", size/1024), func(b *testing.B) {
			for b.Loop() {
				out, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String("bench"), Key: key})
				if err != nil {
					b.Fatal(err)
				}
				if _, err := io.Copy(io.Discard, out.Body); err != nil {
					b.Fatal(err)
				}
				out.Body.Close()
			}
		})
	}
}

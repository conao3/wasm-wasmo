# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-s3"
require "digest"
require "tmpdir"

class S3Test < Minitest::Test
  include WasmoTest

  def setup
    super
    @s3 = @aws.client(Aws::S3::Client)
    @s3.create_bucket(bucket: "bkt")
  end

  def test_binary_round_trip_virtual_hosted
    data = pattern(70_000)
    @s3.put_object(bucket: "bkt", key: "bin/a.dat", body: data)
    out = @s3.get_object(bucket: "bkt", key: "bin/a.dat")
    assert_equal data, out.body.read.b
  end

  def test_path_style
    s3 = @aws.client(Aws::S3::Client, force_path_style: true)
    s3.put_object(bucket: "bkt", key: "p", body: "path")
    assert_equal "path", @s3.get_object(bucket: "bkt", key: "p").body.read
    assert_equal ["p"], s3.list_objects_v2(bucket: "bkt").contents.map(&:key)
  end

  def test_streaming_get
    data = pattern(200_000)
    @s3.put_object(bucket: "bkt", key: "big", body: StringIO.new(data))
    chunks = []
    @s3.get_object(bucket: "bkt", key: "big") { |chunk| chunks << chunk }
    assert_equal data, chunks.join.b
    Dir.mktmpdir do |dir|
      path = File.join(dir, "out")
      @s3.get_object(bucket: "bkt", key: "big", response_target: path)
      assert_equal data, File.binread(path)
    end
  end

  def test_checksums
    out = @s3.put_object(bucket: "bkt", key: "c", body: "checked", checksum_algorithm: "SHA256")
    assert_equal Digest::SHA256.base64digest("checked"), out.checksum_sha256
    got = @s3.get_object(bucket: "bkt", key: "c", checksum_mode: "ENABLED")
    assert_equal "checked", got.body.read
    assert_equal Digest::SHA256.base64digest("checked"), got.checksum_sha256
  end

  def test_range_and_head
    @s3.put_object(bucket: "bkt", key: "r", body: "0123456789")
    assert_equal "234", @s3.get_object(bucket: "bkt", key: "r", range: "bytes=2-4").body.read
    assert_equal 10, @s3.head_object(bucket: "bkt", key: "r").content_length
  end

  def test_missing_key
    assert_raises(Aws::S3::Errors::NoSuchKey) { @s3.get_object(bucket: "bkt", key: "none") }
    assert_raises(Aws::S3::Errors::NotFound) { @s3.head_object(bucket: "bkt", key: "none") }
  end

  def test_multipart_transfer_manager
    data = pattern(12 * 1024 * 1024)
    tm = Aws::S3::TransferManager.new(client: @s3)
    Dir.mktmpdir do |dir|
      src = File.join(dir, "src")
      File.binwrite(src, data)
      tm.upload_file(src, bucket: "bkt", key: "mp", multipart_threshold: 5 * 1024 * 1024)
      head = @s3.head_object(bucket: "bkt", key: "mp")
      assert_equal data.bytesize, head.content_length
      assert_match(/-3"\z/, head.etag)
      dst = File.join(dir, "dst")
      tm.download_file(dst, bucket: "bkt", key: "mp", chunk_size: 5 * 1024 * 1024)
      assert_equal Digest::SHA256.hexdigest(data), Digest::SHA256.file(dst).hexdigest
    end
  end

  def test_presigned_get
    @s3.put_object(bucket: "bkt", key: "shared.txt", body: "signed")
    url = Aws::S3::Presigner.new(client: @s3).presigned_url(:get_object, bucket: "bkt", key: "shared.txt")
    res = @aws.request("GET", url)
    assert_equal 200, res.status
    assert_equal "signed", res.body
    missing = Aws::S3::Presigner.new(client: @s3).presigned_url(:get_object, bucket: "bkt", key: "missing.txt")
    assert_equal 404, @aws.request("GET", missing).status
  end
end

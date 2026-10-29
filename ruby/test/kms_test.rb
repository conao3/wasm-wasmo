# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-kms"

class KMSTest < Minitest::Test
  include WasmoTest

  def test_encrypt_decrypt
    kms = @aws.client(Aws::KMS::Client)
    key = kms.create_key.key_metadata.key_id
    kms.create_alias(alias_name: "alias/app", target_key_id: key)
    secret = pattern(300)
    blob = kms.encrypt(key_id: "alias/app", plaintext: secret, encryption_context: { "app" => "x" }).ciphertext_blob
    refute_equal secret, blob
    out = kms.decrypt(ciphertext_blob: blob, encryption_context: { "app" => "x" })
    assert_equal secret, out.plaintext
    assert_equal key, out.key_id.split("/").last
    assert_raises(Aws::KMS::Errors::InvalidCiphertextException) do
      kms.decrypt(ciphertext_blob: blob, encryption_context: { "app" => "y" })
    end
  end

  def test_data_key
    kms = @aws.client(Aws::KMS::Client)
    key = kms.create_key.key_metadata.key_id
    dk = kms.generate_data_key(key_id: key, key_spec: "AES_256")
    assert_equal 32, dk.plaintext.bytesize
    assert_equal dk.plaintext, kms.decrypt(ciphertext_blob: dk.ciphertext_blob).plaintext
  end
end

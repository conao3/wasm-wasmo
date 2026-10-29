# frozen_string_literal: true

Gem::Specification.new do |spec|
  spec.name = "wasmo"
  spec.version = "0.1.0"
  spec.summary = "Stateful in-process AWS mock for the AWS SDK for Ruby v3, powered by a single WebAssembly module"
  spec.authors = ["conao3"]
  spec.license = "Apache-2.0"
  spec.homepage = "https://github.com/conao3/wasm-wasmo"
  spec.metadata = {
    "source_code_uri" => "https://github.com/conao3/wasm-wasmo/tree/main/ruby",
    "rubygems_mfa_required" => "true"
  }
  spec.required_ruby_version = ">= 3.2"
  spec.files = Dir["lib/**/*.rb"] + ["lib/wasmo/wasmo.wasm", "README.md", "LICENSE", "NOTICE"]
  spec.add_dependency "aws-sdk-core", "~> 3.200"
  spec.add_dependency "fiddle", "~> 1.1"
  spec.add_dependency "wasmtime", ">= 50", "< 60"
end

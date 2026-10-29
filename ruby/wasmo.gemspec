# frozen_string_literal: true

Gem::Specification.new do |spec|
  spec.name = "wasmo"
  spec.version = "0.1.1"
  spec.summary = "Stateful in-process AWS mock for the AWS SDK for Ruby v3, powered by a single WebAssembly module"
  spec.description = <<~DESC
    wasmo runs a stateful AWS mock inside the test process. A Seahorse plugin replaces the HTTP send
    handler of the AWS SDK for Ruby v3 and answers every request from a WebAssembly module run with
    wasmtime-rb, so signing, retries, checksums, and response parsing stay in the SDK. Each mock owns
    an isolated instance, with no Docker, ports, or sidecar process.
  DESC
  spec.authors = ["conao3"]
  spec.license = "Apache-2.0"
  spec.homepage = "https://github.com/conao3/wasm-wasmo"
  spec.metadata = {
    "homepage_uri" => "https://github.com/conao3/wasm-wasmo",
    "source_code_uri" => "https://github.com/conao3/wasm-wasmo/tree/main/ruby",
    "documentation_uri" => "https://github.com/conao3/wasm-wasmo/tree/main/ruby#readme",
    "changelog_uri" => "https://github.com/conao3/wasm-wasmo/blob/main/CHANGELOG.md",
    "bug_tracker_uri" => "https://github.com/conao3/wasm-wasmo/issues",
    "rubygems_mfa_required" => "true"
  }
  spec.required_ruby_version = ">= 3.3"
  spec.files = Dir["lib/**/*.rb"] + ["lib/wasmo/wasmo.wasm", "README.md", "LICENSE", "NOTICE"]
  spec.add_dependency "aws-sdk-core", "~> 3.200"
  spec.add_dependency "fiddle", "~> 1.1"
  spec.add_dependency "wasmtime", ">= 50", "< 60"
end

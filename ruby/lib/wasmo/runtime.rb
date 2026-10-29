# frozen_string_literal: true

require "digest"
require "fileutils"
require "json"
require "securerandom"
require "uri"
require "wasmtime"

module Wasmo
  # The compiled module and host imports, shared by every mock in the process.
  # The machine code is cached under Gem.cache_home/wasmo, so only the first
  # process after an upgrade pays for compilation.
  module Runtime
    WASM_PATH = File.expand_path("wasmo.wasm", __dir__)
    LOAD_LOCK = Mutex.new

    class << self
      def instantiate
        engine, linker, mod = load
        store = Wasmtime::Store.new(engine)
        [store, linker.instantiate(store, mod)]
      end

      private

      def load
        LOAD_LOCK.synchronize do
          @load ||= begin
            engine = Wasmtime::Engine.new
            [engine, linker(engine), compile(engine)]
          end
        end
      end

      def compile(engine)
        bytes = File.binread(WASM_PATH)
        key = Digest::SHA256.hexdigest([bytes, engine.precompile_compatibility_key, Wasmtime::VERSION].join("\0"))
        path = File.join(Gem.cache_home, "wasmo", "#{key}.cwasm")
        return Wasmtime::Module.deserialize_file(engine, path) if File.exist?(path)

        mod = Wasmtime::Module.new(engine, bytes)
        store_cache(path, mod.serialize)
        mod
      end

      def store_cache(path, data)
        dir = File.dirname(path)
        FileUtils.mkdir_p(dir)
        tmp = "#{path}.#{Process.pid}.#{SecureRandom.hex(4)}"
        File.binwrite(tmp, data)
        File.rename(tmp, path)
        FileUtils.rm_f(Dir[File.join(dir, "*.cwasm")] - [path])
      end

      def linker(engine)
        linker = Wasmtime::Linker.new(engine)
        linker.func_new("wasmo", "now_ms", [], [:f64]) do |_caller|
          Process.clock_gettime(Process::CLOCK_REALTIME, :float_millisecond)
        end
        linker.func_new("wasmo", "random_fill", %i[i32 i32], []) do |caller, ptr, len|
          caller.export("memory").to_memory.write(ptr & 0xffff_ffff, SecureRandom.bytes(len & 0xffff_ffff))
          nil
        end
        linker
      end
    end
  end

  # One WebAssembly instance. Calls are serialized because a wasm instance is
  # not reentrant.
  class Instance
    def initialize
      @lock = Mutex.new
      @store, instance = Runtime.instantiate
      @memory = instance.export("memory").to_memory
      @alloc = instance.export("wasmo_alloc").to_func
      @free = instance.export("wasmo_free").to_func
      @handle = instance.export("wasmo_handle").to_func
    end

    def roundtrip(meta, body)
      frame = @lock.synchronize { call(meta, body) }
      meta_len = frame.unpack1("N")
      [JSON.parse(frame.byteslice(4, meta_len)), frame.byteslice((4 + meta_len)..)]
    end

    def closed?
      @lock.synchronize { @store.nil? }
    end

    def close
      @lock.synchronize do
        @store&.close
        @store = nil
      end
    end

    private

    def call(meta, body)
      raise ClosedError, "wasmo: mock is closed" if @store.nil?

      res = @handle.call(put(meta), meta.bytesize, put(body), body.bytesize) & 0xffff_ffff_ffff_ffff
      ptr = res >> 32
      len = res & 0xffff_ffff
      frame = @memory.read(ptr, len)
      @free.call(ptr, len)
      frame
    end

    def put(data)
      ptr = @alloc.call(data.bytesize) & 0xffff_ffff
      @memory.write(ptr, data) unless data.empty?
      ptr
    end
  end
end

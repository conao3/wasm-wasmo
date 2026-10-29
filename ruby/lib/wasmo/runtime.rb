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
        define_invoke(linker)
      end

      def define_invoke(linker)
        linker.func_new("wasmo", "invoke", %i[i32 i32 i32 i32],
                        []) do |caller, meta_ptr, meta_len, payload_ptr, payload_len|
          pending = Thread.current[:wasmo_pending] or raise Error, "wasmo: invoke outside a request"
          memory = caller.export("memory").to_memory
          pending << [memory.read(meta_ptr & 0xffff_ffff, meta_len),
                      memory.read(payload_ptr & 0xffff_ffff, payload_len)]
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
      @complete = instance.export("wasmo_complete").to_func
    end

    # Sends a request and runs the Lambda invocations it starts outside the
    # lock, answering each with wasmo_complete.
    def roundtrip(meta, body, functions)
      pending = []
      frame = @lock.synchronize { call(@handle, meta, body, pending) }
      until pending.empty?
        result, payload = Function.run(functions, *pending.shift)
        answer = @lock.synchronize { call(@complete, result, payload.b, pending) }
        frame = answer if answer
      end
      decode(frame)
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

    def decode(frame)
      meta_len = frame.unpack1("N")
      res = JSON.parse(frame.byteslice(4, meta_len))
      raise Error, "wasmo: invocation #{res['await']} was never answered" if res.key?("await")

      [res, frame.byteslice((4 + meta_len)..)]
    end

    def call(func, meta, body, pending)
      raise ClosedError, "wasmo: mock is closed" if @store.nil?

      res = with_pending(pending) do
        func.call(put(meta), meta.bytesize, put(body), body.bytesize)
      end & 0xffff_ffff_ffff_ffff
      return nil if res.zero?

      ptr = res >> 32
      len = res & 0xffff_ffff
      frame = @memory.read(ptr, len)
      @free.call(ptr, len)
      frame
    end

    def with_pending(pending)
      Thread.current[:wasmo_pending] = pending
      yield
    ensure
      Thread.current[:wasmo_pending] = nil
    end

    def put(data)
      ptr = @alloc.call(data.bytesize) & 0xffff_ffff
      @memory.write(ptr, data) unless data.empty?
      ptr
    end
  end
end

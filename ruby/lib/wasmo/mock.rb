# frozen_string_literal: true

module Wasmo
  # An isolated in-process AWS.
  class Mock
    def initialize
      @instance = Instance.new
      @functions = {}
    end

    # Binds the block to every invocation of the Lambda function named +name+.
    # It receives the decoded event and a FunctionContext and returns a
    # JSON-serializable response; an exception becomes a function error. It
    # runs outside the mock's lock, so it may call AWS through the same mock.
    def register_function(name, &handler)
      raise ArgumentError, "wasmo: register_function needs a block" unless handler

      @functions[name] = handler
    end

    # Client options that send requests to this mock.
    def config
      {
        plugins: [Plugin.new(self)],
        region: REGION,
        endpoint: ENDPOINT,
        credentials: Aws::Credentials.new("test", "test")
      }
    end

    # Builds an AWS SDK client that talks to this mock.
    def client(klass, **options)
      base = config
      klass.new(**base, **options, plugins: base[:plugins] + Array(options[:plugins]))
    end

    # Drops all state.
    def reset
      raise ClosedError, "wasmo: mock is closed" if @instance.closed?

      old = @instance
      @instance = Instance.new
      old.close
    end

    def close
      @instance.close
    end

    Response = Struct.new(:status, :headers, :body)

    # Sends one raw HTTP request to the mock. Response headers are pairs in
    # wire order.
    def request(method, url, headers = [], body = "")
      pairs = headers.map { |k, v| [k.to_s.downcase, v.to_s] }
      pairs << ["host", host(URI(url))] unless pairs.any? { |k, _| k == "host" }
      meta = JSON.generate({ method: method, url: url, headers: pairs })
      res, data = @instance.roundtrip(meta, body.b, @functions)
      Response.new(res["status"], res["headers"], data)
    end

    private

    def host(uri)
      uri.port == uri.default_port ? uri.host : "#{uri.host}:#{uri.port}"
    end
  end
end

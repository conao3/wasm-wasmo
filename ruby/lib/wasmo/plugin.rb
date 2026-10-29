# frozen_string_literal: true

module Wasmo
  # A Seahorse plugin whose send handler delivers requests to a Wasmo::Mock
  # instead of Net::HTTP. Signing, retries and response parsing stay in the
  # SDK.
  class Plugin < Seahorse::Client::Plugin
    def initialize(mock)
      super()
      @mock = mock
    end

    def add_options(config)
      super
      config.add_option(:wasmo_mock, @mock)
    end

    # The send handler.
    class Handler < Seahorse::Client::Handler
      def call(context)
        transmit(context)
        Seahorse::Client::Response.new(context: context)
      end

      private

      def transmit(context)
        res = send_request(context.config.wasmo_mock, context.http_request)
        resp = context.http_response
        resp.signal_headers(res.status, response_headers(res.headers))
        resp.signal_data(res.body) unless res.body.empty?
        resp.signal_done
      rescue StandardError => e
        context.http_response.signal_error(e)
      end

      def send_request(mock, req)
        mock.request(req.http_method, req.endpoint.to_s, req.headers.to_h, read(req.body))
      end

      def read(body)
        return +"" if body.nil?

        body.rewind if body.respond_to?(:rewind)
        data = body.read || +""
        body.rewind if body.respond_to?(:rewind)
        data
      end

      def response_headers(pairs)
        pairs.each_with_object({}) do |(k, v), out|
          name = k.downcase
          out[name] = out.key?(name) ? "#{out[name]}, #{v}" : v
        end
      end
    end

    handler(Handler, step: :send)
  end
end

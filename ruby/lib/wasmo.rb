# frozen_string_literal: true

require "aws-sdk-core"
require_relative "wasmo/runtime"
require_relative "wasmo/function"
require_relative "wasmo/plugin"
require_relative "wasmo/mock"

# A stateful AWS mock that runs inside the Ruby process. Each Wasmo::Mock owns
# an isolated WebAssembly instance, so state never leaks between mocks.
module Wasmo
  ENDPOINT = "http://wasmo.local"
  REGION = "us-east-1"

  class Error < StandardError; end

  class ClosedError < Error; end

  CONFIG_LOCK = Mutex.new

  # Starts a mock, points every AWS client built inside the block at it
  # through Aws.config, and closes it when the block returns.
  def self.mock_aws
    mock = Mock.new
    saved = swap_aws_config { |config| config.merge(mock.config) { |key, old, new| key == :plugins ? old + new : new } }
    begin
      yield mock
    ensure
      swap_aws_config { saved }
      mock.close
    end
  end

  def self.swap_aws_config
    CONFIG_LOCK.synchronize do
      saved = Aws.config
      Aws.config = yield saved
      saved
    end
  end
  private_class_method :swap_aws_config
end

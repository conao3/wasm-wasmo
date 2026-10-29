# frozen_string_literal: true

module Wasmo
  # The invocation a registered handler runs, as the Lambda context object
  # describes it.
  class FunctionContext
    FIELDS = {
      function_name: "functionName",
      function_version: "functionVersion",
      invoked_function_arn: "invokedFunctionArn",
      memory_limit_in_mb: "memoryLimitInMB",
      aws_request_id: "awsRequestId",
      log_group_name: "logGroupName",
      log_stream_name: "logStreamName",
      invocation_type: "invocationType",
      client_context: "clientContext"
    }.freeze

    attr_reader(*FIELDS.keys, :environment)

    def initialize(meta)
      FIELDS.each { |attr, key| instance_variable_set(:"@#{attr}", meta[key]) }
      @environment = meta["environment"].freeze
      @deadline_ms = meta["deadlineMs"]
      @log = +""
      @log_lock = Mutex.new
    end

    def get_remaining_time_in_millis # rubocop:disable Naming/AccessorMethodName
      [(@deadline_ms - Process.clock_gettime(Process::CLOCK_REALTIME, :millisecond)).to_i, 0].max
    end

    # Appends a line to the invocation log that Invoke returns with LogType Tail.
    def log(message)
      @log_lock.synchronize do
        @log << message.to_s
        @log << "\n" unless @log.end_with?("\n")
      end
    end

    def log_text
      @log_lock.synchronize { @log.dup }
    end
  end

  # Runs the invocations the module announces through its invoke import.
  module Function
    def self.run(functions, meta_json, payload)
      meta = JSON.parse(meta_json)
      handler = functions[meta["functionName"]]
      return [JSON.generate({ id: meta["id"], registered: false }), ""] if handler.nil?

      context = FunctionContext.new(meta)
      result, body = answer(handler, payload, context)
      [JSON.generate({ id: meta["id"], **result, log: context.log_text }), body]
    end

    def self.answer(handler, payload, context)
      [{}, JSON.generate(handler.call(JSON.parse(payload), context))]
    rescue StandardError => e
      [{ error: { type: e.class.name, message: e.message, stackTrace: e.backtrace || [] } }, ""]
    end
  end
end

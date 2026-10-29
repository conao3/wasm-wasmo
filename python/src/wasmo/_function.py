from __future__ import annotations

import json
import threading
import time
import traceback
from collections.abc import Callable
from typing import Any

Handler = Callable[[Any, "FunctionContext"], Any]


class FunctionContext:
    """The invocation a registered handler runs, as the Lambda context object describes it."""

    def __init__(self, meta: dict[str, Any]) -> None:
        self.function_name: str = meta["functionName"]
        self.function_version: str = meta["functionVersion"]
        self.invoked_function_arn: str = meta["invokedFunctionArn"]
        self.memory_limit_in_mb: int = meta["memoryLimitInMB"]
        self.aws_request_id: str = meta["awsRequestId"]
        self.log_group_name: str = meta["logGroupName"]
        self.log_stream_name: str = meta["logStreamName"]
        self.invocation_type: str = meta["invocationType"]
        self.client_context: str | None = meta.get("clientContext")
        self.environment: dict[str, str] = dict(meta["environment"])
        self._deadline_ms: float = meta["deadlineMs"]
        self._lines: list[str] = []
        self._lock = threading.Lock()

    def get_remaining_time_in_millis(self) -> int:
        return max(0, int(self._deadline_ms - time.time() * 1000))

    def log(self, message: str) -> None:
        """Append a line to the invocation log that Invoke returns with LogType Tail."""
        with self._lock:
            self._lines.append(message if message.endswith("\n") else message + "\n")

    def _log_text(self) -> str:
        with self._lock:
            return "".join(self._lines)


def run(handlers: dict[str, Handler], meta_bytes: bytes, payload: bytes) -> tuple[bytes, bytes]:
    """Run one invocation and encode the result meta and payload for wasmo_complete."""
    meta = json.loads(meta_bytes)
    handler = handlers.get(meta["functionName"])
    if handler is None:
        return json.dumps({"id": meta["id"], "registered": False}).encode(), b""
    context = FunctionContext(meta)
    result: dict[str, Any] = {"id": meta["id"]}
    body = b""
    try:
        body = json.dumps(handler(json.loads(payload), context)).encode()
    except Exception as e:
        result["error"] = {
            "type": type(e).__name__,
            "message": str(e),
            "stackTrace": [line.rstrip("\n") for line in traceback.format_tb(e.__traceback__)],
        }
    result["log"] = context._log_text()
    return json.dumps(result).encode(), body

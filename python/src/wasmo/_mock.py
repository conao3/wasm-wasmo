from __future__ import annotations

import functools
import inspect
import io
import json
import os
import struct
import threading
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass
from typing import Any, TypeVar, overload
from urllib.parse import urlsplit

import boto3
import botocore.loaders
import botocore.session
from botocore.awsrequest import AWSPreparedRequest, AWSResponse

from wasmo._runtime import Instance

ENDPOINT = "http://wasmo.local"
REGION = "us-east-1"
ACCESS_KEY_ID = "test"
SECRET_ACCESS_KEY = "test"

_ENV = {
    "AWS_ACCESS_KEY_ID": ACCESS_KEY_ID,
    "AWS_SECRET_ACCESS_KEY": SECRET_ACCESS_KEY,
    "AWS_SESSION_TOKEN": None,
    "AWS_SECURITY_TOKEN": None,
    "AWS_DEFAULT_REGION": REGION,
    "AWS_REGION": REGION,
}

_loader = botocore.loaders.create_loader()
_create_client = botocore.session.Session.create_client
_create_client_signature = inspect.signature(_create_client)

F = TypeVar("F", bound=Callable[..., Any])


@dataclass(frozen=True)
class Response:
    status: int
    headers: dict[str, str]
    body: bytes


class _RawResponse(io.BytesIO):
    def stream(self, amt: int = 65536, decode_content: bool | None = None) -> Iterator[bytes]:
        while chunk := self.read(amt):
            yield chunk


def _bytes(body: Any) -> bytes:
    if body is None:
        return b""
    if isinstance(body, (bytes, bytearray, memoryview)):
        return bytes(body)
    if isinstance(body, str):
        return body.encode()
    if hasattr(body, "read"):
        return _bytes(body.read())
    if isinstance(body, Iterable):
        return b"".join(_bytes(chunk) for chunk in body)
    raise TypeError(f"wasmo: unsupported request body {type(body).__name__}")


def _header(value: str | bytes) -> str:
    return value.decode() if isinstance(value, bytes) else value


class Mock:
    """An isolated in-process AWS backed by its own WebAssembly instance."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._instance: Instance | None = None
        self._closed = False

    def fetch(
        self,
        url: str,
        *,
        method: str = "GET",
        headers: dict[str, str] | None = None,
        body: bytes = b"",
    ) -> Response:
        """Send a raw HTTP request, such as a presigned URL, to this mock."""
        pairs = [(k.lower(), v) for k, v in (headers or {}).items()]
        if not any(k == "host" for k, _ in pairs):
            pairs.append(("host", urlsplit(url).netloc))
        meta = json.dumps({"method": method, "url": url, "headers": pairs}).encode()
        with self._lock:
            if self._closed:
                raise RuntimeError("wasmo: mock is closed")
            if self._instance is None:
                self._instance = Instance()
            frame = self._instance.handle(meta, body)
        (meta_len,) = struct.unpack_from(">I", frame)
        res = json.loads(frame[4 : 4 + meta_len])
        out: dict[str, str] = {}
        for name, value in res["headers"]:
            out[name] = f"{out[name]}, {value}" if name in out else value
        return Response(res["status"], out, frame[4 + meta_len :])

    def _send(self, request: AWSPreparedRequest, **_: Any) -> AWSResponse:
        headers = {k: _header(v) for k, v in request.headers.items()}
        res = self.fetch(
            request.url, method=request.method, headers=headers, body=_bytes(request.body)
        )
        return AWSResponse(request.url, res.status, res.headers, _RawResponse(res.body))

    def _bind(self, client: Any) -> Any:
        client.meta.events.register("before-send", self._send, unique_id=f"wasmo-{id(self)}")
        return client

    def session(self, **kwargs: Any) -> boto3.Session:
        """A boto3 session whose clients and resources talk to this mock."""
        kwargs.setdefault("region_name", REGION)
        if "aws_access_key_id" not in kwargs:
            kwargs["aws_access_key_id"] = ACCESS_KEY_ID
            kwargs["aws_secret_access_key"] = SECRET_ACCESS_KEY
        return boto3.Session(botocore_session=_Session(self), **kwargs)

    def client(self, service_name: str, **kwargs: Any) -> Any:
        return self.session().client(service_name, **kwargs)

    def resource(self, service_name: str, **kwargs: Any) -> Any:
        return self.session().resource(service_name, **kwargs)

    def reset(self) -> None:
        """Drop all state. Existing clients keep working against the empty state."""
        with self._lock:
            if self._closed:
                raise RuntimeError("wasmo: mock is closed")
            self._instance = None

    def close(self) -> None:
        with self._lock:
            self._closed = True
            self._instance = None

    def start(self) -> None:
        """Route every botocore client created from now on to this mock."""
        _activate(self)

    def stop(self) -> None:
        _deactivate(self)

    def __enter__(self) -> Mock:
        self.start()
        return self

    def __exit__(self, *_: object) -> None:
        self.stop()
        self.close()

    def __call__(self, func: F) -> F:
        return mock_aws(func)


class _Session(botocore.session.Session):
    def __init__(self, mock: Mock) -> None:
        super().__init__()
        self.register_component("data_loader", _loader)
        self._wasmo = mock

    def create_client(self, *args: Any, **kwargs: Any) -> Any:
        bound = _create_client_signature.bind(self, *args, **kwargs)
        if bound.arguments.get("endpoint_url") is None:
            bound.arguments["endpoint_url"] = ENDPOINT
        return self._wasmo._bind(_create_client(*bound.args, **bound.kwargs))


_active: list[Mock] = []
_active_lock = threading.Lock()
_saved_env: dict[str, str | None] = {}


def _patched_create_client(session: botocore.session.Session, *args: Any, **kwargs: Any) -> Any:
    with _active_lock:
        mock = _active[-1] if _active else None
    bound = _create_client_signature.bind(session, *args, **kwargs)
    if mock is None:
        return _create_client(*bound.args, **bound.kwargs)
    arguments = bound.arguments
    if arguments.get("endpoint_url") is None:
        arguments["endpoint_url"] = ENDPOINT
    if arguments.get("aws_access_key_id") is None:
        arguments["aws_access_key_id"] = ACCESS_KEY_ID
        arguments["aws_secret_access_key"] = SECRET_ACCESS_KEY
        arguments["aws_session_token"] = None
    return mock._bind(_create_client(*bound.args, **bound.kwargs))


def _activate(mock: Mock) -> None:
    with _active_lock:
        if mock in _active:
            raise RuntimeError("wasmo: mock is already started")
        if not _active:
            for name, value in _ENV.items():
                _saved_env[name] = os.environ.get(name)
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
            botocore.session.Session.create_client = _patched_create_client  # type: ignore[method-assign]
        _active.append(mock)


def _deactivate(mock: Mock) -> None:
    with _active_lock:
        if mock not in _active:
            raise RuntimeError("wasmo: mock is not started")
        _active.remove(mock)
        if not _active:
            botocore.session.Session.create_client = _create_client  # type: ignore[method-assign]
            for name, value in _saved_env.items():
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
            _saved_env.clear()


@overload
def mock_aws() -> Mock: ...


@overload
def mock_aws(func: F) -> F: ...


def mock_aws(func: F | None = None) -> Mock | F:
    """Return a Mock for ``with``, or wrap ``func`` so each call runs in a fresh Mock."""
    if func is None:
        return Mock()

    @functools.wraps(func)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        with Mock():
            return func(*args, **kwargs)

    return wrapper  # type: ignore[return-value]

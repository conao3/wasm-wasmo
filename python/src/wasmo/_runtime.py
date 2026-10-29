from __future__ import annotations

import ctypes
import os
import threading
import time
from importlib.resources import files

import wasmtime

_compile_lock = threading.Lock()
_compiled: tuple[wasmtime.Engine, wasmtime.Module] | None = None


def _module() -> tuple[wasmtime.Engine, wasmtime.Module]:
    global _compiled
    with _compile_lock:
        if _compiled is None:
            engine = wasmtime.Engine()
            binary = files(__package__).joinpath("wasmo.wasm").read_bytes()
            _compiled = (engine, wasmtime.Module(engine, binary))
        return _compiled


def _now_ms() -> float:
    return time.time() * 1000


def _random_fill(caller: wasmtime.Caller, ptr: int, length: int) -> None:
    memory = caller["memory"]
    assert isinstance(memory, wasmtime.Memory)
    memory.write(caller, os.urandom(length & 0xFFFFFFFF), ptr & 0xFFFFFFFF)


def _read(caller: wasmtime.Caller, ptr: int, length: int) -> bytes:
    memory = caller["memory"]
    assert isinstance(memory, wasmtime.Memory)
    ptr, length = ptr & 0xFFFFFFFF, length & 0xFFFFFFFF
    return bytes(memory.read(caller, ptr, ptr + length))


class Instance:
    def __init__(self) -> None:
        engine, module = _module()
        self._store = wasmtime.Store(engine)
        self._pending: list[tuple[bytes, bytes]] | None = None
        linker = wasmtime.Linker(engine)
        linker.define_func(
            "wasmo", "now_ms", wasmtime.FuncType([], [wasmtime.ValType.f64()]), _now_ms
        )
        linker.define_func(
            "wasmo",
            "random_fill",
            wasmtime.FuncType([wasmtime.ValType.i32(), wasmtime.ValType.i32()], []),
            _random_fill,
            access_caller=True,
        )
        i32 = wasmtime.ValType.i32()
        linker.define_func(
            "wasmo",
            "invoke",
            wasmtime.FuncType([i32, i32, i32, i32], []),
            self._invoke,
            access_caller=True,
        )
        exports = linker.instantiate(self._store, module).exports(self._store)
        self._memory = exports["memory"]
        self._alloc = exports["wasmo_alloc"]
        self._free = exports["wasmo_free"]
        self._handle = exports["wasmo_handle"]
        self._complete = exports["wasmo_complete"]

    def _invoke(
        self,
        caller: wasmtime.Caller,
        meta_ptr: int,
        meta_len: int,
        payload_ptr: int,
        payload_len: int,
    ) -> None:
        if self._pending is None:
            raise RuntimeError("wasmo: invoke outside a request")
        self._pending.append(
            (_read(caller, meta_ptr, meta_len), _read(caller, payload_ptr, payload_len))
        )

    def _span(self, ptr: int, length: int) -> int:
        if ptr + length > self._memory.data_len(self._store):
            raise RuntimeError("wasmo: memory access out of range")
        return ctypes.addressof(self._memory.data_ptr(self._store).contents) + ptr

    def _put(self, data: bytes) -> int:
        ptr = self._alloc(self._store, len(data)) & 0xFFFFFFFF
        if data:
            ctypes.memmove(self._span(ptr, len(data)), data, len(data))
        return ptr

    def handle(self, meta: bytes, body: bytes, pending: list[tuple[bytes, bytes]]) -> bytes:
        frame = self._call(self._handle, meta, body, pending)
        assert frame is not None
        return frame

    def complete(
        self, meta: bytes, payload: bytes, pending: list[tuple[bytes, bytes]]
    ) -> bytes | None:
        return self._call(self._complete, meta, payload, pending)

    def _call(
        self, fn: wasmtime.Func, meta: bytes, body: bytes, pending: list[tuple[bytes, bytes]]
    ) -> bytes | None:
        meta_ptr = self._put(meta)
        body_ptr = self._put(body)
        self._pending = pending
        try:
            res = fn(self._store, meta_ptr, len(meta), body_ptr, len(body))
        finally:
            self._pending = None
        if res == 0:
            return None
        ptr, length = (res >> 32) & 0xFFFFFFFF, res & 0xFFFFFFFF
        frame = ctypes.string_at(self._span(ptr, length), length)
        self._free(self._store, ptr, length)
        return frame

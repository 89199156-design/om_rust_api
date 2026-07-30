from __future__ import annotations

import ctypes
import ctypes.util
import os
from pathlib import Path


class TurboPForDecoder:
    """ctypes wrapper for a native library exposing p4nddec64."""

    def __init__(self, library):
        self._library = library
        self._p4nddec64 = library.p4nddec64
        try:
            self._p4nddec64.argtypes = [
                ctypes.POINTER(ctypes.c_ubyte),
                ctypes.c_size_t,
                ctypes.POINTER(ctypes.c_uint64),
            ]
            self._p4nddec64.restype = ctypes.c_size_t
        except AttributeError:
            # Test doubles may not support ctypes attributes.
            pass

    @classmethod
    def from_library_path(cls, path: Path | str) -> "TurboPForDecoder":
        return cls(ctypes.CDLL(str(path)))

    def decode_u64_delta0(self, payload: bytes, *, count: int) -> tuple[list[int], int]:
        if count < 0:
            raise ValueError("count must be non-negative")
        if count == 0:
            return [], 0
        if not payload:
            raise ValueError("payload must not be empty")

        input_buffer = (ctypes.c_ubyte * len(payload)).from_buffer_copy(payload)
        output_buffer = (ctypes.c_uint64 * count)()
        consumed = int(
            self._p4nddec64(
                input_buffer,
                ctypes.c_size_t(count),
                output_buffer,
            )
        )
        if consumed <= 0 or consumed > len(payload):
            raise ValueError("native p4nddec64 returned invalid compressed byte count")
        return [int(output_buffer[index]) for index in range(count)], consumed


def load_default_turbopfor_decoder() -> TurboPForDecoder:
    path = os.environ.get("OM_TURBOPFOR_LIB")
    if path:
        return TurboPForDecoder.from_library_path(path)

    found = ctypes.util.find_library("turbopfor")
    if found:
        return TurboPForDecoder.from_library_path(found)

    raise NotImplementedError(
        "OM v3 compressed LUT decoding requires a native TurboPFor p4nddec64 library; "
        "set OM_TURBOPFOR_LIB to its shared-library path"
    )

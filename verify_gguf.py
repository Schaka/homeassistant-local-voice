#!/usr/bin/env python3
"""Reject truncated/corrupt GGUF model files before they reach the native loader.

A GGUF header (magic, metadata, tensor-info section) sits at the front of the
file, so a download that got cut short still parses as "valid" -- only the
tensor payload is missing. parakeet.cpp / audio.cpp have no obligation to
detect that; they trust the header's declared tensor sizes. Run this against
every model path in the entrypoint, before starting either engine, so a short
file fails fast and loud instead of driving a garbage-sized GPU allocation.

Unrecognized ggml tensor types are skipped rather than failing the check, so
newer quant types this table doesn't know about never produce a false
positive.
"""
import os
import struct
import sys

# (block_size, type_size_bytes) for the ggml types this project's models use.
_TYPE_SIZES = {
    0: (1, 4),      # F32
    1: (1, 2),      # F16
    2: (32, 18),    # Q4_0
    3: (32, 20),    # Q4_1
    6: (32, 22),    # Q5_0
    7: (32, 24),    # Q5_1
    8: (32, 34),    # Q8_0
    9: (32, 36),    # Q8_1
    10: (256, 84),  # Q2_K
    11: (256, 110), # Q3_K
    12: (256, 144), # Q4_K
    13: (256, 176), # Q5_K
    14: (256, 210), # Q6_K
    15: (256, 292), # Q8_K
    24: (1, 1),     # I8
    25: (1, 2),     # I16
    26: (1, 4),     # I32
    27: (1, 8),     # I64
    28: (1, 8),     # F64
    30: (1, 2),     # BF16
}

_GGUF_STRING = 8
_GGUF_ARRAY = 9
_SCALAR_SIZES = {0: 1, 1: 1, 2: 2, 3: 2, 4: 4, 5: 4, 6: 4, 7: 1, 10: 8, 11: 8, 12: 8}


class _Reader:
    def __init__(self, fh):
        self._fh = fh

    def read(self, n: int) -> bytes:
        b = self._fh.read(n)
        if len(b) != n:
            raise EOFError("header truncated")
        return b

    def u32(self) -> int:
        return struct.unpack("<I", self.read(4))[0]

    def u64(self) -> int:
        return struct.unpack("<Q", self.read(8))[0]

    def gguf_string(self) -> bytes:
        return self.read(self.u64())

    def skip_value(self, vtype: int) -> None:
        if vtype in _SCALAR_SIZES:
            self.read(_SCALAR_SIZES[vtype])
        elif vtype == _GGUF_STRING:
            self.gguf_string()
        elif vtype == _GGUF_ARRAY:
            elem_type = self.u32()
            for _ in range(self.u64()):
                self.skip_value(elem_type)
        else:
            raise ValueError(f"unknown GGUF value type {vtype}")


def expected_min_size(path: str) -> int:
    """Header-declared minimum file size: data section start + last tensor's end."""
    with open(path, "rb") as fh:
        r = _Reader(fh)
        if r.read(4) != b"GGUF":
            raise ValueError("not a GGUF file (bad magic)")
        r.u32()  # version
        tensor_count = r.u64()
        kv_count = r.u64()

        alignment = 32
        for _ in range(kv_count):
            key = r.gguf_string()
            vtype = r.u32()
            if key == b"general.alignment" and vtype in _SCALAR_SIZES:
                alignment = int.from_bytes(r.read(_SCALAR_SIZES[vtype]), "little")
            else:
                r.skip_value(vtype)

        max_end = 0
        for _ in range(tensor_count):
            r.gguf_string()  # name
            dims = [r.u64() for _ in range(r.u32())]
            ttype = r.u32()
            offset = r.u64()
            if ttype in _TYPE_SIZES:
                block, tsize = _TYPE_SIZES[ttype]
                n_elems = 1
                for d in dims:
                    n_elems *= d
                max_end = max(max_end, offset + (n_elems // block) * tsize)

        data_start = fh.tell()
        if data_start % alignment:
            data_start += alignment - (data_start % alignment)
        return data_start + max_end


def check(path: str) -> bool:
    if not os.path.isfile(path):
        print(f"MISSING {path}", file=sys.stderr)
        return False
    try:
        want = expected_min_size(path)
    except (EOFError, ValueError) as err:
        print(f"CORRUPT {path}: {err}", file=sys.stderr)
        return False
    got = os.path.getsize(path)
    if got < want:
        print(
            f"TRUNCATED {path}: header declares at least {want} bytes, "
            f"file is {got} bytes ({want - got} bytes short)",
            file=sys.stderr,
        )
        return False
    return True


if __name__ == "__main__":
    ok = all(check(p) for p in sys.argv[1:])
    sys.exit(0 if ok else 1)

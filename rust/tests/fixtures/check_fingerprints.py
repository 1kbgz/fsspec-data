"""Independent Python reference for the documented version-one byte protocol.

Run without arguments to verify fixtures; --write updates expected digests.
No Python binding or Arrow dependency is needed for these scalar fixtures.
"""

import hashlib
import json
import sys
from pathlib import Path


def fingerprint(case):
    h = hashlib.sha256(b"fsspec-data/batch/1\0")

    def integer(value, size=8, signed=False):
        h.update(value.to_bytes(size, "little", signed=signed))

    def blob(value):
        integer(len(value))
        h.update(value)

    def metadata(values):
        integer(len(values))
        for key, value in sorted(values.items()):
            blob(key.encode())
            blob(value.encode())

    integer(case["position"])
    blob(b"rows")
    integer(len(case["columns"][0]["values"]))
    integer(len(case["columns"]))
    metadata(case["metadata"])
    for column in case["columns"]:
        blob(column["name"].encode())
        blob(json.dumps(column["data_type"], ensure_ascii=False, separators=(",", ":")).encode())
        integer(int(column["nullable"]), 1)
        metadata(column["metadata"])
    for column in case["columns"]:
        dtype = column["data_type"]
        for value in column["values"]:
            integer(int(value is not None), 1)
            if value is None:
                continue
            if dtype == "Int64":
                integer(value, signed=True)
            elif dtype == "Utf8":
                blob(value.encode())
            elif dtype == "Boolean":
                integer(int(value), 1)
            elif dtype == "Float64":
                integer(int(value, 16))
            elif dtype == "Binary":
                blob(bytes.fromhex(value))
            elif isinstance(dtype, dict) and "Timestamp" in dtype:
                integer(value, signed=True)
            elif isinstance(dtype, dict) and "Decimal128" in dtype:
                integer(value, 16, signed=True)
            else:
                raise ValueError(dtype)
    return h.hexdigest()


path = Path(__file__).with_name("batch_fingerprints.json")
cases = json.loads(path.read_text())
if sys.argv[1:] == ["--write"]:
    for case in cases:
        case["sha256"] = fingerprint(case)
    path.write_text(json.dumps(cases, ensure_ascii=False, indent=2) + "\n")
else:
    for case in cases:
        assert fingerprint(case) == case["sha256"], case["name"]
    print(f"{len(cases)} fingerprint fixtures verified")

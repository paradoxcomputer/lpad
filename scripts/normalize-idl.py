#!/usr/bin/env python3
"""Normalize a SPEL IDL JSON for order-insensitive comparison: the idl-gen
collects helper account-types in hash order, so sort the `types` array by name
and emit with sorted keys."""
import json, sys
d = json.load(open(sys.argv[1]))
if isinstance(d.get("types"), list):
    d["types"] = sorted(d["types"], key=lambda t: t.get("name", ""))
print(json.dumps(d, indent=2, sort_keys=True))

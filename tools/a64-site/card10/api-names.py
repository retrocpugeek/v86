"""The card10 firmware's API ids and interrupt numbers, by name, from its
epicardium/epicardium.h (for the page's view of the cores' interaction):

    python3 api-names.py epicardium.h > api.json

{"calls": {"<id>": "epic_disp_update", ...}, "interrupts": {"<n>": "CTRL_C", ...}}
"""
import json
import re
import sys

text = open(sys.argv[1]).read()
ids = {name: int(v, 0) for name, v in re.findall(r"^#define (API_[A-Z0-9_]+)\s+(0x[0-9A-Fa-f]+|\d+)", text, re.M)}
calls = {}
# API(API_X, <return type> epic_name(...));
for name, decl in re.findall(r"^API(?:_ISR)?\(\s*(API_[A-Z0-9_]+)\s*,\s*([^;]*?\()", text, re.M | re.S):
    fn = re.search(r"(\w+)\s*\($", decl.strip())
    if name in ids and fn:
        calls[str(ids[name])] = fn.group(1)
interrupts = {str(int(v)): name for name, v in re.findall(r"^#define EPIC_INT_([A-Z0-9_]+)\s+(\d+)", text, re.M) if name != "NUM"}
json.dump({"calls": calls, "interrupts": interrupts}, sys.stdout, indent=0, sort_keys=True)
print()

#!/usr/bin/env python3
"""Marks Node modules as ported in PORTING.md: mark-ported.py core/x.js=x.rs ...
The Rust path is relative to crates/ai-dev-companion/src/."""
import re, sys
text = open("PORTING.md").read().split("\n")
want = dict(a.split("=", 1) for a in sys.argv[1:])
for i, ln in enumerate(text):
    m = re.match(r"\| \[.\] \| `([^`]+)` \| (\d+) \| (\d+) \|", ln)
    if m and m[1] in want:
        text[i] = f"| [x] | `{m[1]}` | {m[2]} | {m[3]} | `crates/ai-dev-companion/src/{want[m[1]]}` |"
        del want[m[1]]
open("PORTING.md", "w").write("\n".join(text))
if want:
    print("not found in the table:", ", ".join(want))

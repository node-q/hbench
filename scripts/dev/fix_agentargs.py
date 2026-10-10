# Insert `load_trajectory: None,` into AgentArgs struct literals that lack it.
import re, sys, pathlib
for p in pathlib.Path("src").rglob("*.rs"):
    s = p.read_text(); out = []; i = 0; changed = False
    for m in re.finditer(r"AgentArgs \{", s):
        start = m.end()
        depth, j = 1, start
        while depth:
            depth += {"{": 1, "}": -1}.get(s[j], 0); j += 1
        body = s[start:j]
        prev = s[max(0, m.start()-12):m.start()]
        if "load_trajectory" not in body and not re.search(r"(pub struct |impl |-> |for )$", prev) and ".." not in body.split("\n")[-1]:
            out.append(s[i:start] + " load_trajectory: None,"); i = start; changed = True
    out.append(s[i:])
    if changed:
        p.write_text("".join(out)); print("fixed", p)

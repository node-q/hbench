# Derive each agent's accepted kwargs (options_model fields incl. inherited) from the Python agent sources via AST.
import ast, pathlib, json, sys
root = pathlib.Path(sys.argv[1])
classes = {}  # name -> (bases, fields, options_model, agent_name)
for p in list(root.rglob("*.py")):
    try: tree = ast.parse(p.read_text())
    except Exception: continue
    for n in ast.walk(tree):
        if not isinstance(n, ast.ClassDef): continue
        bases = [b.id if isinstance(b, ast.Name) else (b.attr if isinstance(b, ast.Attribute) else None) for b in n.bases]
        fields, om, name = [], None, None
        for s in n.body:
            if isinstance(s, ast.AnnAssign) and isinstance(s.target, ast.Name) and not s.target.id.startswith("_"):
                ann = ast.unparse(s.annotation)
                if "ClassVar" not in ann:
                    aliases = []
                    if isinstance(s.value, ast.Call):
                        for kw in s.value.keywords:
                            if kw.arg in ("alias", "validation_alias"):
                                aliases += [c.value for c in ast.walk(kw.value) if isinstance(c, ast.Constant) and isinstance(c.value, str)]
                    fields.extend(aliases if aliases else [s.target.id])
            if isinstance(s, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "options_model" for t in s.targets):
                om = ast.unparse(s.value)
            if isinstance(s, ast.FunctionDef) and s.name == "name":
                for r in ast.walk(s):
                    if isinstance(r, ast.Attribute) and isinstance(r.value, ast.Attribute) and r.value.attr == "AgentName" or (isinstance(r, ast.Attribute) and isinstance(r.value, ast.Name) and r.value.id == "AgentName"):
                        name = r.attr
                    if isinstance(r, ast.Constant) and isinstance(r.value, str) and name is None: name = "STR:" + r.value
        classes.setdefault(n.name, (bases, fields, om, name))
enum = {}
for line in (root.parent / "models/agent/name.py").read_text().splitlines():
    if "=" in line and '"' in line:
        k, v = line.split("=", 1); enum[k.strip()] = v.strip().strip('"')
def fields_of(c, seen=()):
    if c not in classes or c in seen: return []
    b, f, _, _ = classes[c]
    out = []
    for x in b:
        if x and x not in ("BaseModel",): out += fields_of(x, seen + (c,))
    return out + f
def om_of(c, seen=()):
    if c not in classes or c in seen: return None
    b, _, om, _ = classes[c]
    if om: return om
    for x in b:
        r = om_of(x, seen + (c,))
        if r: return r
def agent_name(c, seen=()):
    if c not in classes or c in seen: return None
    b, _, _, nm = classes[c]
    if nm: return enum.get(nm, nm[4:] if nm.startswith("STR:") else nm)
    return None
out = {}
for c in classes:
    nm, om = agent_name(c), om_of(c)
    if nm and om and nm in enum.values():
        out[nm] = sorted(set(fields_of(om)))
print(json.dumps(dict(sorted(out.items())), indent=1))

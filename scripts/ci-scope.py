#!/usr/bin/env python3
"""What CI has to run on macOS, derived from the code.

    scripts/ci-scope.py macos-crates     # the crates that can behave differently on macOS, and why
    scripts/ci-scope.py check            # .config/macos-tests.txt names exactly those crates, and
                                         # software-mode.yml's PR paths take what its build reads
    scripts/ci-scope.py changes [--base <rev> | --base-tree <dir>] < files
                                         # which jobs a change list needs (key=value lines)

The `linux` job runs the whole workspace's tests; the macOS `lint-test` job lints the whole
workspace (cfg(macos) code only compiles there) but tests only the crates in
`.config/macos-tests.txt`: those that can behave differently on macOS. A crate is in when one of
its compiled files (on macOS) has a `cfg` that differs between aarch64 macOS and x86_64 Linux
(`#[cfg]`, `#![cfg]`, `cfg!`, `cfg_attr` other than lint levels), when its Cargo.toml has a
macOS-only dependency, when it carries Objective-C, Swift or C with `__APPLE__`, or when it builds
on macOS and depends (normal or dev) on a crate that is in. Code gated off on macOS
(`#[cfg(target_os = "linux")] mod soft;`, emu-kvm) doesn't count. `check` fails when a crate is
missing from the list (or listed without reason), so the macOS test run can't silently drop one.
`check` also derives what Software mode's `cargo build -p` compiles on Linux (`cargo metadata`,
then the module trees) and fails when its pull_request paths skip a file that build reads, or
name a crate it doesn't build.

`changes` reads changed paths (one per line; renames as both paths) and prints `lint`,
`desktop`, `emu` and `macos` (desktop or emu) as true/false: whether the change can affect the
macOS lint-test job, the desktop workspace (macOS and Linux), or the macOS release build of emu.
Docs, Linux-only Rust files, scripts no CI job runs and other workflows affect none of them; a
script counts for the jobs whose config or code names it (and the scripts it names, in turn),
and any file the code a job builds include_str!s or include_bytes!s counts for that job. A cfg
is evaluated three-valued: a predicate the target doesn't fix (a feature, `test`,
`debug_assertions`) is unknown, and code whose gate comes out unknown counts as compiled.
Each path is judged in this tree and in the --base tree (the diff's other side) and the answers
are OR-ed, so a deleted, renamed or newly gated file counts by what it was too. Scoping fails
safe: a path it doesn't know, a path in neither tree, a name a job uses that points at no file,
or anything it can't read or parse (a tree, a cfg, an argument) affects all. Stdlib only; runs
on Python 3.8+.
"""

import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
LIST = ".config/macos-tests.txt"

MAC = {"target_os": "macos", "target_vendor": "apple", "target_family": "unix", "target_arch": "aarch64",
       "target_env": "", "target_pointer_width": "64", "target_endian": "little", "unix": True, "windows": False}
LINUX = {"target_os": "linux", "target_vendor": "unknown", "target_family": "unix", "target_arch": "x86_64",
         "target_env": "gnu", "target_pointer_width": "64", "target_endian": "little", "unix": True, "windows": False}
LINT_ATTRS = ("allow", "expect", "warn", "deny", "forbid")


# --- cfg expressions -------------------------------------------------------------------------

def parse_cfg(text):
    """`all(target_os = "macos", not(test))` -> nested tuples: ("all", [...]), ("kv", k, v), ("name", n)."""
    text = text.strip()
    if text.startswith("(") and text.endswith(")"):
        text = text[1:-1]
    toks = re.findall(r'"(?:[^"\\]|\\.)*"|[A-Za-z_][A-Za-z0-9_]*|[(),=]', text)
    pos = [0]

    def peek():
        return toks[pos[0]] if pos[0] < len(toks) else None

    def take():
        if pos[0] >= len(toks):
            raise ValueError("can't parse cfg(%s)" % text)
        pos[0] += 1
        return toks[pos[0] - 1]

    def pred():
        name = take()
        if peek() == "(":
            take()
            args = []
            while peek() != ")":
                args.append(pred())
                if peek() == ",":
                    take()
            take()
            return (name, args)
        if peek() == "=":
            take()
            return ("kv", name, take().strip('"'))
        return ("name", name)

    e = pred()
    if pos[0] != len(toks):
        raise ValueError("can't parse cfg(%s)" % text)
    return e


def ev3(e, env):
    """cfg(e) on env: True, False, or None when it turns on a predicate env doesn't fix (a
    feature, `test` and `debug_assertions` outside an override: both builds exist)."""
    kind = e[0]
    if kind in ("all", "any"):
        vals = [ev3(a, env) for a in e[1]]
        decides = kind == "any"  # any: one True decides it; all: one False does
        if decides in vals:
            return decides
        return None if None in vals else not decides
    if kind == "not":
        v = ev3(e[1][0], env) if len(e[1]) == 1 else None
        return None if v is None else not v
    if kind == "kv":
        return env[e[1]] == e[2] if e[1] in env else None
    if kind == "name":
        return env[e[1]] if e[1] in env else None
    return None


def present(e, env, overrides=None):
    """Whether code under cfg(e) can be compiled for env: unless it's surely off there."""
    return ev3(e, dict(env, **(overrides or {}))) is not False


def unknowns(e, env, out):
    """The predicates in e that env doesn't fix, by repr."""
    if e[0] in ("all", "any", "not"):
        for a in e[1]:
            unknowns(a, env, out)
    elif e[0] not in ("kv", "name") or e[1] not in env:
        out.add(repr(e))
    return out


def ev(e, env, assign):
    """cfg(e) on env with the predicates it doesn't fix set by assign ({repr: bool})."""
    kind = e[0]
    if kind == "all":
        return all(ev(a, env, assign) for a in e[1])
    if kind == "any":
        return any(ev(a, env, assign) for a in e[1])
    if kind == "not":
        return len(e[1]) != 1 or not ev(e[1][0], env, assign)
    if kind in ("kv", "name") and e[1] in env:
        return env[e[1]] == e[2] if kind == "kv" else env[e[1]]
    return assign[repr(e)]


def on_mac_vs_linux(e, test):
    """Whether test(on macOS, on Linux) holds for some value of the predicates neither fixes
    (the same value on both: a feature or `test` is the same choice there); too many to try
    counts as yes."""
    names = sorted(unknowns(e, MAC, unknowns(e, LINUX, set())))
    if len(names) > 12:
        return True
    for bits in range(1 << len(names)):
        assign = {n: bool(bits >> i & 1) for i, n in enumerate(names)}
        if test(ev(e, MAC, assign), ev(e, LINUX, assign)):
            return True
    return False


def differs(e):
    return on_mac_vs_linux(e, lambda m, l: m != l)


def mac_only(e):
    """Code under cfg(e) is compiled on macOS and not on Linux, for some value of the unknowns."""
    return on_mac_vs_linux(e, lambda m, l: m and not l)


# --- Rust sources ----------------------------------------------------------------------------

def strip_comments(src):
    """Blanks comments (keeping newlines, so offsets keep their line numbers); strings stay."""
    out, i, n = [], 0, len(src)
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(re.sub(r"[^\n]", " ", src[i:j]))
            i = j
        elif c == "r" and re.match(r'r#*"', src[i:i + 8]) and not (i and (src[i - 1].isalnum() or src[i - 1] == "_")):
            hashes = re.match(r"r(#*)\"", src[i:]).group(1)
            end = src.find('"' + hashes, i + 2 + len(hashes))
            j = n if end < 0 else end + 1 + len(hashes)
            out.append(src[i:j])
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            out.append(src[i:j + 1])
            i = j + 1
        elif c == "'":
            m = re.match(r"'(?:\\.[^']*|[^\\'])'", src[i:i + 12])
            j = i + (m.end() if m else 1)
            out.append(src[i:j])
            i = j
        else:
            out.append(c)
            i += 1
    return "".join(out)


def balanced(text, i, open_ch, close_ch):
    """Index just past the bracket closing the one at text[i]."""
    depth, j = 0, i
    while j < len(text):
        if text[j] == '"':
            j += 1
            while j < len(text) and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
        elif text[j] == open_ch:
            depth += 1
        elif text[j] == close_ch:
            depth -= 1
            if depth == 0:
                return j + 1
        j += 1
    return len(text)


ATTR = re.compile(r"#!?\[")
MOD_DECL = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")
MOD_LINE = re.compile(r"(?m)^[ \t]*((?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;)")
USE_DECL = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?(?:use\b|extern\s+crate\b)")


def attributes(text):
    """Every attribute: (start, end, inner, is_inner, following) where following is the text
    offset of the item the attribute run applies to."""
    attrs = []
    for m in ATTR.finditer(text):
        start = m.start()
        end = balanced(text, m.end() - 1, "[", "]")
        attrs.append([start, end, text[m.end():end - 1].strip(), text[start + 1] == "!", None])
    # The item an attribute applies to starts after the attribute run it belongs to.
    for a in attrs:
        j = a[1]
        while True:
            while j < len(text) and text[j].isspace():
                j += 1
            nxt = [b for b in attrs if b[0] == j and not b[3]]
            if not nxt:
                break
            j = nxt[0][1]
        a[4] = j
    return attrs


def scan_rust(path, text=None):
    """(mod declarations, cfg uses) of one file. A declaration is (name, [cfg exprs], path attr);
    a cfg use is (line, kind, expr) with kind 'file', 'item' or 'expr'."""
    if text is None:
        with open(path, encoding="utf-8", errors="replace") as f:
            text = strip_comments(f.read())
    attrs = attributes(text)
    line = lambda off: text.count("\n", 0, off) + 1
    mods, uses = [], []
    # The file's own inner attributes are those before its first item; a #![cfg] further down is
    # inside an inline module or a body, and gates only that.
    file_attrs, j = set(), 0
    while True:
        while j < len(text) and text[j].isspace():
            j += 1
        at = next((a for a in attrs if a[0] == j and a[3]), None)
        if not at:
            break
        file_attrs.add(j)
        j = at[1]
    by_item = {}
    for start, end, inner, is_inner, item in attrs:
        if not is_inner:
            by_item.setdefault(item, []).append(inner)
    for m in MOD_LINE.finditer(text):
        cfgs, p = [], None
        for inner in by_item.get(m.start(1), []):
            if re.match(r"cfg\s*\(", inner):
                cfgs.append(parse_cfg(inner[inner.index("("):]))
            pm = re.match(r'path\s*=\s*"([^"]+)"', inner)
            if pm:
                p = pm.group(1)
        mods.append((m.group(2), cfgs, p))
    for start, end, inner, is_inner, item in attrs:
        head = re.match(r"(cfg_attr|cfg)\s*\(", inner)
        if not head:
            continue
        open_at = inner.index("(")
        body = inner[open_at:balanced(inner, open_at, "(", ")")]
        if head.group(1) == "cfg_attr":
            parts = split_top(body[1:-1])
            if all(re.match(r"(%s)\s*\(" % "|".join(LINT_ATTRS), p.strip()) for p in parts[1:]):
                continue
            uses.append((line(start), "expr", parse_cfg(parts[0])))
        elif is_inner:
            uses.append((line(start), "file" if start in file_attrs else "item", parse_cfg(body)))
        elif MOD_DECL.match(text, item):
            continue  # a gated child module: the module walk handles it
        elif USE_DECL.match(text, item):
            continue  # a gated import changes no behaviour by itself
        else:
            uses.append((line(start), "item", parse_cfg(body)))
    for m in re.finditer(r"\bcfg!\s*\(", text):
        o = m.end() - 1
        uses.append((line(m.start()), "expr", parse_cfg(text[o:balanced(text, o, "(", ")")])))
    return mods, uses, text


def split_top(s):
    parts, depth, cur = [], 0, []
    for ch in s:
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    parts.append("".join(cur))
    return [p for p in parts if p.strip()]


# --- the workspace ---------------------------------------------------------------------------

def read(rel):
    with open(os.path.join(ROOT, rel), encoding="utf-8", errors="replace") as f:
        return f.read()


def toml_sections(text):
    """[(header, [lines])] for a Cargo.toml, comments dropped."""
    out, header = [], ""
    out.append((header, []))
    for raw in text.splitlines():
        ln = raw.split("#", 1)[0].strip() if not raw.strip().startswith("[") else raw.strip()
        if ln.startswith("["):
            header = ln.strip("[]").strip()
            out.append((header, []))
        elif ln:
            out[-1][1].append(ln)
    return out


class Crate:
    def __init__(self, dir_):
        self.dir = dir_
        text = read(os.path.join(dir_, "Cargo.toml"))
        self.sections = toml_sections(text)
        self.name = next(re.match(r'name\s*=\s*"([^"]+)"', ln).group(1)
                         for h, lines in self.sections if h == "package" for ln in lines if ln.startswith("name"))
        self.bins = [re.match(r'path\s*=\s*"([^"]+)"', ln).group(1)
                     for h, lines in self.sections if h == "bin" for ln in lines if ln.startswith("path")]

    def deps(self, dev=False):
        """[(dependency name, target cfg or None)] from the build, normal (and dev) sections."""
        kinds = ("dependencies", "build-dependencies") + (("dev-dependencies",) if dev else ())
        out = []
        for h, lines in self.sections:
            target = None
            m = re.match(r"target\.'?(.+?)'?\.(dependencies|build-dependencies|dev-dependencies)$", h)
            if m:
                target, kind = m.group(1), m.group(2)
            else:
                kind = h
            if kind not in kinds:
                continue
            for ln in lines:
                dm = re.match(r"([A-Za-z0-9_-]+)\s*(?:\.\s*workspace\s*=|=)", ln)
                if dm:
                    out.append((package_of(dm.group(1), ln), target))
        return out


def package_of(key, line):
    """The package a dependency line names: its `package = "..."`, else the root's
    `[workspace.dependencies]` entry's for a `.workspace = true` key, else the key itself (the
    workspace crates are published as `simee-*` under their old keys)."""
    m = re.search(r'\bpackage\s*=\s*"([^"]+)"', line)
    if m:
        return m.group(1)
    if re.match(r"[A-Za-z0-9_-]+\s*\.\s*workspace\s*=|[A-Za-z0-9_-]+\s*=\s*\{[^}]*\bworkspace\s*=\s*true", line):
        return workspace_packages().get(key, key)
    return key


_WORKSPACE_PACKAGES = {}


def workspace_packages():
    """{dependency key: package} for the renamed `[workspace.dependencies]` of the tree at ROOT."""
    if ROOT not in _WORKSPACE_PACKAGES:
        out = _WORKSPACE_PACKAGES[ROOT] = {}
        for h, lines in toml_sections(read("Cargo.toml")):
            if h == "workspace.dependencies":
                for ln in lines:
                    m = re.match(r'([A-Za-z0-9_-]+)\s*=\s*\{[^}]*\bpackage\s*=\s*"([^"]+)"', ln)
                    if m:
                        out[m.group(1)] = m.group(2)
    return _WORKSPACE_PACKAGES[ROOT]


def workspace():
    """{package name: Crate} for the root workspace's members."""
    text = read("Cargo.toml")
    members = re.search(r"members\s*=\s*\[([^\]]*)\]", text).group(1)
    crates = {}
    for pat in re.findall(r'"([^"]+)"', members):
        if pat.endswith("/*"):
            base = pat[:-2]
            dirs = sorted(os.path.join(base, d) for d in os.listdir(os.path.join(ROOT, base)))
        else:
            dirs = [pat]
        for d in dirs:
            if os.path.isfile(os.path.join(ROOT, d, "Cargo.toml")):
                c = Crate(d)
                crates[c.name] = c
    return crates


def walk_files(dir_):
    for base, dirs, files in os.walk(os.path.join(ROOT, dir_)):
        dirs[:] = sorted(d for d in dirs if d not in ("target", ".git", "node_modules"))
        for f in sorted(files):
            yield os.path.relpath(os.path.join(base, f), ROOT)


def roots(crate):
    """(file, kind) for each compilation root: lib, bin, test, example, bench, build."""
    out = []
    d = crate.dir
    for rel, kind in (("src/lib.rs", "lib"), ("src/main.rs", "bin"), ("build.rs", "build")):
        if os.path.isfile(os.path.join(ROOT, d, rel)):
            out.append((os.path.join(d, rel), kind))
    for b in crate.bins:
        p = os.path.join(d, b)
        if (p, "bin") not in out and os.path.isfile(os.path.join(ROOT, p)):
            out.append((p, "bin"))
    for sub, kind in (("src/bin", "bin"), ("tests", "test"), ("examples", "example"), ("benches", "bench")):
        full = os.path.join(ROOT, d, sub)
        if os.path.isdir(full):
            for f in sorted(os.listdir(full)):
                p = os.path.join(d, sub, f)
                if f.endswith(".rs"):
                    out.append((p, kind))
                elif os.path.isfile(os.path.join(ROOT, p, "main.rs")):
                    out.append((os.path.join(p, "main.rs"), kind))
    return out


_scans = {}


def scan(path):
    if path not in _scans:
        _scans[path] = scan_rust(os.path.join(ROOT, path))
    return _scans[path]


def module_tree(root, env, overrides=None, reached=None):
    """{file: compiled?} for every file reachable from root on env. A file is compiled when every
    gate on its way (mod-declaration cfgs, the files' own #![cfg]) holds there. `reached`, when
    given, collects {file: compiled but for its own #![cfg]?}."""
    seen, reach = {}, {} if reached is None else reached

    def visit(path, on, is_dir_owner):
        if path in seen and (reach.get(path) or not on):
            return
        reach[path] = reach.get(path, False) or on
        mods, uses, _ = scan(path)
        if on:
            for _, kind, e in uses:
                if kind == "file" and not present(e, env, overrides):
                    on = False
        seen[path] = on or seen.get(path, False)
        here = os.path.dirname(path)
        own = here if is_dir_owner else os.path.join(here, os.path.splitext(os.path.basename(path))[0])
        for name, cfgs, p in mods:
            child_on = on and all(present(e, env, overrides) for e in cfgs)
            if p:
                cands = [os.path.normpath(os.path.join(here, p))]
            else:
                cands = [os.path.join(own, name + ".rs"), os.path.join(own, name, "mod.rs")]
            for c in cands:
                if os.path.isfile(os.path.join(ROOT, c)):
                    base = os.path.basename(c)
                    visit(c, child_on, base in ("mod.rs", "lib.rs", "main.rs") or bool(p) and base == "mod.rs")
                    break

    visit(root, True, True)
    return seen


def compiled(crate, env, kinds=("lib", "bin", "test", "example", "bench", "build"), overrides=None, own_gate=True):
    """{file: compiled?} over the crate's roots of the given kinds; with own_gate=False, whether
    each file would be compiled were it not for its own #![cfg] (so whether editing only that
    file can change what's compiled)."""
    out = {}
    for root, kind in roots(crate):
        if kind not in kinds:
            continue
        if kind == "build":
            out[root] = True
            continue
        reached = {}
        tree = module_tree(root, env, overrides, reached)
        for f, on in (tree if own_gate else reached).items():
            out[f] = out.get(f, False) or on
    return out


APPLE_SOURCE = re.compile(r"__APPLE__|__MACH__|TARGET_OS_")


def macos_reasons(crate):
    """Why the crate's code can behave differently on macOS: ['file:line why', ...]."""
    reasons = []
    files = compiled(crate, MAC, kinds=("lib", "bin", "test", "build"))
    for f in sorted(files):
        if not files[f]:
            continue
        mods, uses, text = scan(f)
        for ln, kind, e in uses:
            if (kind == "expr" and differs(e)) or (kind in ("item", "file") and mac_only(e)):
                reasons.append("%s:%d cfg" % (f, ln))
                break
        if f.endswith("build.rs") and re.search(r"CARGO_CFG_TARGET_(OS|VENDOR|FAMILY)", text):
            reasons.append("%s target-dependent build script" % f)
        # A macOS-only child module counts as macOS code even when it has no cfg of its own.
        for name, cfgs, p in mods:
            if cfgs and any(mac_only(e) for e in cfgs):
                reasons.append("%s mod %s is macOS-only" % (f, name))
                break
    for h, lines in crate.sections:
        m = re.match(r"target\.'?(.+?)'?\.(dependencies|build-dependencies|dev-dependencies)$", h)
        if not m:
            continue
        t = m.group(1)
        if t.startswith("cfg("):
            if mac_only(parse_cfg(t[3:])):
                reasons.append("%s/Cargo.toml [%s]" % (crate.dir, h))
        elif "apple" in t:
            reasons.append("%s/Cargo.toml [%s]" % (crate.dir, h))
    for f in walk_files(crate.dir):
        ext = os.path.splitext(f)[1]
        if ext in (".m", ".mm", ".swift", ".entitlements"):
            reasons.append("%s Apple source" % f)
        elif ext in (".c", ".h", ".cc", ".cpp"):
            with open(os.path.join(ROOT, f), encoding="utf-8", errors="replace") as fh:
                if APPLE_SOURCE.search(fh.read()):
                    reasons.append("%s __APPLE__" % f)
    return reasons


def read_list():
    """{crate: comment} from .config/macos-tests.txt."""
    out = {}
    for raw in read(LIST).splitlines():
        body, _, comment = raw.partition("#")
        name = body.strip()
        if name:
            out[name] = comment.strip()
    return out


def macos_crates(crates):
    """{crate: reasons} for every crate the macOS job tests: those with macOS-specific code of
    their own, and those that build on macOS and depend (normal or dev, directly or through
    another such crate) on one: a dependency's macOS behaviour reaches them at run time without a
    cfg of theirs (simee's socket paths, sim-mcp through sim-api)."""
    derived = {n: macos_reasons(c) for n, c in crates.items()}
    derived = {n: r for n, r in derived.items() if r}
    builds = {n: any(on for f, on in compiled(c, MAC, kinds=("lib", "bin", "test")).items())
              for n, c in crates.items()}
    grew = True
    while grew:
        grew = False
        for n, c in sorted(crates.items()):
            if n in derived or not builds[n]:
                continue
            via = sorted({d for d, _ in c.deps(dev=True) if d in derived})
            if via:
                derived[n] = ["depends on %s" % ", ".join(via)]
                grew = True
    return derived


def cmd_macos_crates():
    for name, r in sorted(macos_crates(workspace()).items()):
        print("%-14s %s%s" % (name, r[0], " (+%d)" % (len(r) - 1) if len(r) > 1 else ""))
    return 0


def cmd_check():
    crates = workspace()
    listed = read_list()
    found = macos_crates(crates)
    derived = {n: found.get(n, []) for n in crates}
    errors = []
    for n in sorted(listed):
        if n not in crates:
            errors.append("%s names %s, which is not a workspace package" % (LIST, n))
        elif not derived[n] and not listed[n]:
            errors.append("%s: %s has no macOS-specific code; drop it, or say why it stays in a comment" % (LIST, n))
    for n in sorted(derived):
        if derived[n] and n not in listed:
            errors.append("%s: %s can behave differently on macOS (%s) but the macOS job doesn't test it: add it"
                          % (LIST, n, "; ".join(derived[n][:3])))
    sw_errors, sw_crates = software_errors(crates)
    errors += sw_errors
    for e in errors:
        print("ci-scope: " + e, file=sys.stderr)
    if not errors:
        print("ci-scope: %s matches the %d crates that can behave differently on macOS" % (LIST, sum(1 for v in derived.values() if v)))
        print("ci-scope: %s's pull_request paths take every file its build of %d crates reads on Linux"
              % (SOFTWARE, len(sw_crates)))
    return 1 if errors else 0


# --- Software mode's pull_request paths ------------------------------------------------------

SOFTWARE = ".github/workflows/software-mode.yml"
# Files outside the crates that change what Software mode builds or how.
SOFTWARE_ALWAYS = ("Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".cargo", ".github/actions/rust-toolchain", SOFTWARE)
INCLUDE = re.compile(r'\binclude_(?:str|bytes)!\s*\(\s*("(?:[^"\\]|\\.)*"\s*\))?')


def glob_re(pattern):
    """A GitHub paths-filter pattern as a regex: `*` stays within a directory, `**` doesn't."""
    out, i = "", 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out, i = out + "(?:.*/)?", i + 3
        elif pattern.startswith("**", i):
            out, i = out + ".*", i + 2
        elif pattern[i] == "*":
            out, i = out + "[^/]*", i + 1
        elif pattern[i] == "?":
            out, i = out + "[^/]", i + 1
        else:
            out, i = out + re.escape(pattern[i]), i + 1
    return re.compile("^" + out + "$")


def filter_runs(patterns, path):
    """Whether a paths filter takes path: the last pattern that matches wins, `!` excludes.
    The patterns and git name files with `/`, so a path joined by Windows's `os.path` does too."""
    path = path.replace(os.sep, "/")
    hit = False
    for pat in patterns:
        if glob_re(pat.lstrip("!")).match(path):
            hit = not pat.startswith("!")
    return hit


def software_filter(text):
    """(pull_request paths, packages its `cargo build -p` builds) from software-mode.yml."""
    lines = text.splitlines()
    at = next(i for i, ln in enumerate(lines) if re.match(r"\s+pull_request:\s*$", ln))
    if not re.match(r"\s+paths:\s*$", lines[at + 1]):
        raise ValueError("%s: pull_request has no paths list" % SOFTWARE)
    patterns = []
    for ln in lines[at + 2:]:
        m = re.match(r'\s+-\s+"([^"]+)"\s*(?:#.*)?$', ln)
        if m:
            patterns.append(m.group(1))
        elif ln.strip() and not ln.strip().startswith("#"):
            break
    builds = re.findall(r"^\s*(?:-\s+run:\s+)?cargo build\b(.*)$", text, re.M)
    packages = sorted({p for b in builds for p in re.findall(r"-p\s+(\S+)", b)})
    if not patterns or not packages:
        raise ValueError("%s: can't read its pull_request paths or its cargo build line" % SOFTWARE)
    return patterns, packages


def linux_closure(packages):
    """The workspace packages `cargo build -p <packages>` compiles on x86_64 Linux, from
    `cargo metadata` (path dependencies, normal and build, whose target holds on Linux)."""
    out = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"], cwd=ROOT,
                         check=True, stdout=subprocess.PIPE).stdout
    pkgs = {p["name"]: p for p in json.loads(out.decode())["packages"]}
    seen, todo = set(), list(packages)
    while todo:
        n = todo.pop()
        if n in seen:
            continue
        if n not in pkgs:
            raise ValueError("%s builds %s, which is not a workspace package" % (SOFTWARE, n))
        seen.add(n)
        for d in pkgs[n]["dependencies"]:
            t = d.get("target")
            if not d.get("path") or d.get("kind") == "dev":
                continue
            if t and not (present(parse_cfg(t[3:]), LINUX) if t.startswith("cfg(") else "linux" in t):
                continue
            todo.append(d["name"])
    return seen


def gated_spans(text, env, overrides=None):
    """[(start, end)] of the items whose #[cfg] is surely off on env."""
    spans = []
    for start, end, inner, is_inner, item in attributes(text):
        if is_inner or not re.match(r"cfg\s*\(", inner):
            continue
        o = inner.index("(")
        if present(parse_cfg(inner[o:balanced(inner, o, "(", ")")]), env, overrides):
            continue
        brace, semi = text.find("{", item), text.find(";", item)
        spans.append((item, semi if brace < 0 or 0 <= semi < brace else balanced(text, brace, "{", "}")))
    return spans


def includes(f, env, overrides=None):
    """[(path or None, line)] for what f's include_str!/include_bytes! read on env outside the
    items gated off there; None is a path ci-scope can't read (not a string literal)."""
    text = scan(f)[2]
    spans = None
    out = []
    for m in INCLUDE.finditer(text):
        spans = gated_spans(text, env, overrides) if spans is None else spans
        if any(a <= m.start() < b for a, b in spans):
            continue
        lit = m.group(1).rstrip(")").strip()[1:-1] if m.group(1) else None
        out.append((os.path.normpath(os.path.join(os.path.dirname(f), lit)) if lit else None,
                    text.count("\n", 0, m.start()) + 1))
    return out


def software_inputs(crates, names):
    """Every file whose change can change what Software mode builds: the toolchain and lockfile,
    each built crate's Cargo.toml and build script, every .rs file its Linux build reaches (one
    gated off only by its own #![cfg] counts: editing it alone can turn it on), and what those
    files include_str!/include_bytes! outside their tests."""
    out = set()
    for f in SOFTWARE_ALWAYS:
        out |= set(walk_files(f)) if os.path.isdir(os.path.join(ROOT, f)) else {f}
    build = dict(kinds=("lib", "bin", "build"), overrides={"test": False})
    for n in sorted(names):
        c = crates[n]
        out.add(os.path.join(c.dir, "Cargo.toml"))
        out |= {f for f, on in compiled(c, LINUX, own_gate=False, **build).items() if on}
        for f, on in compiled(c, LINUX, **build).items():
            if not on or not f.endswith(".rs"):
                continue
            for p, ln in includes(f, LINUX, {"test": False}):
                if not p:
                    raise ValueError("%s:%d includes a path ci-scope can't read" % (f, ln))
                out.add(p)
    return {f.replace(os.sep, "/") for f in out}


def software_errors(crates):
    """(errors, built crates): software-mode.yml's pull_request paths must take every file its
    build reads on Linux, and name no crate it doesn't build."""
    patterns, packages = software_filter(read(SOFTWARE))
    names = linux_closure(packages)
    errors = []
    for f in sorted(software_inputs(crates, names)):
        if not filter_runs(patterns, f):
            errors.append("%s: a pull request changing %s skips Software mode, but `cargo build -p %s` builds it on "
                          "Linux: include it (or narrow the exclusion that drops it)" % (SOFTWARE, f, " -p ".join(packages)))
    dirs = {c.dir: n for n, c in crates.items()}
    for pat in patterns:
        m = re.match(r"((?:apps|crates)/[^/*]+)/\*\*$", pat)
        if m and m.group(1) in dirs and dirs[m.group(1)] not in names:
            errors.append("%s: pull_request paths name %s, which its build doesn't compile on Linux: drop it" % (SOFTWARE, pat))
    return errors, names


# --- what a change list needs ----------------------------------------------------------------

def closure(crates, start, dev=False):
    """Workspace packages start depends on (normal and build deps, any target), start included."""
    seen, todo = set(), [start]
    while todo:
        n = todo.pop()
        if n in seen or n not in crates:
            continue
        seen.add(n)
        todo.extend(d for d, _ in crates[n].deps(dev=dev))
    return seen


def desktop_deps(crates):
    names = set()
    for h, lines in toml_sections(read("desktop/Cargo.toml")):
        if h in ("dependencies", "build-dependencies") or re.match(r"target\..*\.dependencies$", h):
            for ln in lines:
                m = re.match(r'([A-Za-z0-9_-]+)\s*=\s*\{[^}]*path\s*=', ln)
                if m:
                    names.add(package_of(m.group(1), ln))
    out = set()
    for n in names:
        out |= closure(crates, n)
    return out


REFERENCED = re.compile(r'"(?:\.\./)*((?:docs|scripts)/[A-Za-z0-9_./-]+)"')

# Paths no job here reads: docs, other workflows, the website (its own workspace and workflow,
# website.yml), local tooling, the script and docs linters' configs, and scripts nothing below
# names.
NONE = re.compile(r"^(docs/|website/|\.github/workflows/(?:software-mode|release|website)\.yml$|\.githooks/|\.local/|LICENSE$|\.gitignore$"
                  r"|Makefile$|bacon\.toml$|rustfmt\.toml$|\.editorconfig$|\.shellcheckrc$|\.yamllint\.yml$"
                  r"|lychee\.toml$|_typos\.toml$)|\.md$|^scripts/")
# Scripts only the docs-scripts job runs: it runs on every change, so they reach no scoped job.
LINT_ONLY = {"scripts/lint.sh"}
EVERYTHING = re.compile(r"^(\.github/workflows/ci\.yml|\.github/actions/|\.cargo/|rust-toolchain\.toml$|Cargo\.toml$"
                        r"|scripts/ci-scope\.py$)")
# What runs scripts in these jobs: the workflow and its actions (every job), and cargo's config
# (its runner, scripts/sign-run.sh, runs the macOS tests and the desktop's).
SCRIPT_USERS = ((".github/workflows/ci.yml", {"lint", "desktop", "emu"}), (".github/actions", {"lint", "desktop", "emu"}),
                (".cargo/config.toml", {"lint", "desktop"}))
SCRIPT_REF = re.compile(r"(?<![\w./-])scripts/[\w.-]+(?:/[\w.-]+)*")
SHELL = re.compile(r"#!\s*/\S*(sh|bash)\b|#!\s*/usr/bin/env\s+(sh|bash)\b")
# A shell variable holding the script's own directory (sign-run.sh's `here="$(cd "$(dirname "$0")" && pwd)"`).
HERE = re.compile(r"(?m)^\s*(?:local\s+|readonly\s+)?(\w+)=.*\bdirname\b.*\$\{?0\b")


def script_scopes(rust_refs):
    """({scripts/<file>: jobs}, [unresolved]) for the scripts a job runs: those the workflow, its
    actions or cargo's config name, those the tests name (rust_refs), and those a used script
    names in turn (sign-run.sh's hypervisor.entitlements). A name that points at no file in this
    tree is unresolved."""
    scripts = [f for f in walk_files("scripts") if os.path.isfile(os.path.join(ROOT, f))]
    named = lambda text, f: re.search(r"(?<![\w.-])%s(?![\w.-])" % re.escape(os.path.basename(f)), text)
    exists = lambda f: os.path.lexists(os.path.join(ROOT, f))
    out, unresolved = {}, []
    for user, jobs in SCRIPT_USERS:
        users = walk_files(user) if os.path.isdir(os.path.join(ROOT, user)) else [user]
        for u in users:
            if not os.path.isfile(os.path.join(ROOT, u)):
                continue
            for m in SCRIPT_REF.finditer(read(u)):
                ref = m.group(0).rstrip(".")
                if ref in LINT_ONLY:
                    continue
                if exists(ref):
                    out.setdefault(ref, set()).update(jobs)
                else:
                    unresolved.append("%s names %s" % (u, ref))
    for f in sorted(rust_refs):
        if f.startswith("scripts/"):
            if exists(f):
                out.setdefault(f, set()).add("lint")
            else:
                unresolved.append("%s names %s" % (sorted(rust_refs[f])[0], f))
    shell = lambda f: os.path.isfile(os.path.join(ROOT, f)) and SHELL.match(read(f))
    grew = True
    while grew:
        grew = False
        for user in sorted(out):
            if not shell(user):
                continue  # only shell scripts run what they name; this one merely mentions some
            text = read(user)
            for f in scripts:
                if f != user and named(text, f) and not out[user] <= out.get(f, set()):
                    out.setdefault(f, set()).update(out[user])
                    grew = True
    # What a used script runs from its own directory has to be there.
    for user in sorted(out):
        if not shell(user):
            continue
        text = read(user)
        for var in sorted(set(HERE.findall(text))):
            for m in re.finditer(r"\$\{?%s\}?/([\w.-]+)" % var, text):
                ref = os.path.normpath(os.path.join(os.path.dirname(user), m.group(1)))
                if not exists(ref):
                    unresolved.append("%s names %s" % (user, ref))
    return out, unresolved


JOBS = ("lint", "desktop", "emu")


def tree_scopes(paths):
    """({job: [path]}, paths this tree has) for the tree at ROOT. A path this tree doesn't have
    is left to the other tree, which judges what it was (deleted) or is (added)."""
    crates = workspace()
    by_dir = sorted(((c.dir + "/", n) for n, c in crates.items()), key=lambda t: -len(t[0]))
    emu = closure(crates, "simee-emu" if "simee-emu" in crates else "emu")
    desk = desktop_deps(crates)
    on_mac, release, desk_build, refs = {}, {}, {}, {}
    for n, c in crates.items():
        on_mac.update(compiled(c, MAC))
        if n in emu:
            release.update(compiled(c, MAC, kinds=("lib", "bin", "build"), overrides={"test": False}))
        if n in desk:
            for env in (MAC, LINUX):
                for f, on in compiled(c, env, kinds=("lib", "build"), overrides={"test": False}).items():
                    desk_build[f] = desk_build.get(f, False) or on
    for f, on in on_mac.items():
        if on:
            for m in REFERENCED.finditer(scan(f)[2]):
                refs.setdefault(m.group(1), set()).add(f)
    used_scripts, unresolved = script_scopes(refs)
    # What the code a job builds include_str!s or include_bytes!s counts for that job, whatever
    # the file is (a .md included is read like a source); an include it can't resolve, all jobs.
    included = {}
    for job, files, envs in (("lint", on_mac, ((MAC, None),)), ("emu", release, ((MAC, {"test": False}),)),
                             ("desktop", desk_build, ((MAC, {"test": False}), (LINUX, {"test": False})))):
        for f, on in files.items():
            if not on or not f.endswith(".rs"):
                continue
            for env, overrides in envs:
                for p, ln in includes(f, env, overrides):
                    if p and not p.startswith("..") and os.path.isfile(os.path.join(ROOT, p)):
                        included.setdefault(p, set()).add(job)
                    else:
                        unresolved.append("%s:%d includes %s" % (f, ln, p or "a path ci-scope can't read"))
    need = {k: ["(unresolved: %s)" % u for u in dict.fromkeys(unresolved)] for k in JOBS}
    here = set()
    for p in paths:
        if not os.path.lexists(os.path.join(ROOT, p)):
            continue
        here.add(p)
        hit = set(included.get(p, ()))
        owner = next((n for d, n in by_dir if p.startswith(d)), None)
        if p in refs:
            hit.add("lint")
        if EVERYTHING.match(p):
            hit |= set(JOBS)
        elif p == "Cargo.lock" or p.startswith(".config/"):
            hit |= {"lint", "emu"} if p == "Cargo.lock" else {"lint"}
        elif p in used_scripts:
            hit |= used_scripts[p]
        elif p.startswith("desktop/"):
            if not p.endswith(".md"):
                hit.add("desktop")
        elif owner:
            if not p.endswith(".md"):
                if p.endswith(".rs") and p in on_mac:
                    if on_mac[p]:
                        hit.add("lint")
                else:
                    hit.add("lint")
                rel = p[len(crates[owner].dir) + 1:]
                outside_build = rel.split("/")[0] in ("tests", "examples", "benches")
                # A Rust file no module tree reaches (not declared yet) counts; one the trees
                # know is in a build only if a build compiles it.
                unknown = not (p.endswith(".rs") and p in on_mac)
                if owner in emu and not outside_build and release.get(p, unknown):
                    hit.add("emu")
                if owner in desk and not outside_build and desk_build.get(p, unknown):
                    hit.add("desktop")
        elif not NONE.search(p):
            hit |= set(JOBS)
        for k in hit:
            need[k].append(p)
    return need, here


def at_root(root, fn, *args):
    """fn(*args) with ROOT pointing at another tree."""
    global ROOT
    saved, ROOT = ROOT, root
    _scans.clear()
    try:
        return fn(*args)
    finally:
        ROOT = saved
        _scans.clear()


def scopes(paths, trees=None):
    """{job: [path]}: a path needs a job when it does in either tree (the diff's base and head),
    so a renamed, deleted or newly gated file is judged by what it was too. A path no tree has
    can't be judged and needs every job."""
    paths = [p.strip() for p in paths if p.strip()]
    need, seen = {k: [] for k in JOBS}, set()
    for tree in trees or [ROOT]:
        got, here = at_root(tree, tree_scopes, paths)
        seen |= here
        for k in JOBS:
            need[k] += [p for p in got[k] if p not in need[k]]
    for p in paths:
        if p not in seen:
            for k in JOBS:
                need[k].append(p)
    return need


def extract(rev, dest):
    """The tree at rev, written to dest."""
    data = subprocess.run(["git", "-C", ROOT, "archive", "--format=tar", rev], check=True, stdout=subprocess.PIPE).stdout
    with tarfile.open(fileobj=io.BytesIO(data)) as tf:
        if hasattr(tarfile, "data_filter"):
            tf.extractall(dest, filter="data")
        else:
            tf.extractall(dest)
    return dest


def changed(args, paths):
    """scopes() over this tree and each --base <rev> / --base-tree <dir>."""
    trees, tmp = [ROOT], None
    try:
        i = 0
        while i < len(args):
            if args[i] not in ("--base", "--base-tree") or i + 1 >= len(args):
                raise ValueError("unknown argument %s" % args[i])
            if args[i] == "--base":
                tmp = tmp or tempfile.mkdtemp(prefix="ci-scope-")
                trees.append(extract(args[i + 1], os.path.join(tmp, str(i))))
            else:
                if not os.path.isfile(os.path.join(args[i + 1], "Cargo.toml")):
                    raise ValueError("%s is not a tree of this workspace" % args[i + 1])
                trees.append(os.path.abspath(args[i + 1]))
            i += 2
        return scopes(paths, trees)
    finally:
        if tmp:
            shutil.rmtree(tmp, ignore_errors=True)


def cmd_changes(args):
    if "--all" in args:
        need = {k: ["(all)"] for k in JOBS}
    else:
        # Fail safe: a change it can't scope (a tree it can't read, a cfg it can't parse, an
        # argument it doesn't know) runs everything.
        try:
            need = changed(args, sys.stdin.read().splitlines())
        except Exception as e:
            print("ci-scope: can't scope this change (%s: %s): running everything" % (type(e).__name__, e), file=sys.stderr)
            need = {k: ["(can't scope)"] for k in JOBS}
    for k in JOBS:
        print("%s=%s" % (k, "true" if need[k] else "false"))
        why = ", ".join(need[k][:5]) + (" ..." if len(need[k]) > 5 else "") if need[k] else "nothing it reads changed"
        print("ci-scope: %s: %s" % (k, why), file=sys.stderr)
    print("macos=%s" % ("true" if need["desktop"] or need["emu"] else "false"))
    return 0


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0 if argv else 2
    cmd, args = argv[0], argv[1:]
    if cmd == "macos-crates":
        return cmd_macos_crates()
    if cmd == "check":
        return cmd_check()
    if cmd == "changes":
        return cmd_changes(args)
    print("ci-scope: unknown command %s" % cmd, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

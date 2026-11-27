#!/usr/bin/env python3
"""What CI has to run on macOS, derived from the code.

    scripts/ci-scope.py macos-crates     # the crates that can behave differently on macOS, and why
    scripts/ci-scope.py check            # .config/macos-tests.txt names exactly those crates
    scripts/ci-scope.py changes < files  # which jobs a change list needs (key=value lines)

The `linux` job runs the whole workspace's tests; the macOS `lint-test` job lints the whole
workspace (cfg(macos) code only compiles there) but tests only the crates in
`.config/macos-tests.txt`: those that can behave differently on macOS. A crate is in when one of
its compiled files (on macOS) has a `cfg` that differs between aarch64 macOS and x86_64 Linux
(`#[cfg]`, `#![cfg]`, `cfg!`, `cfg_attr` other than lint levels), when its Cargo.toml has a
macOS-only dependency, when it carries Objective-C, Swift or C with `__APPLE__`, or when it builds
on macOS and depends (normal or dev) on a crate that is in. Code gated off on macOS
(`#[cfg(target_os = "linux")] mod soft;`, emu-kvm) doesn't count. `check` fails when a crate is
missing from the list (or listed without reason), so the macOS test run can't silently drop one.

`changes` reads changed paths (one per line; renames as both paths) and prints `lint`,
`desktop`, `emu` and `macos` (desktop or emu) as true/false: whether the change can affect the
macOS lint-test job, the desktop workspace (macOS and Linux), or the macOS release build of emu.
Docs, Linux-only Rust files, scripts no CI job runs and other workflows affect none of them; a
script counts for the jobs whose config or code names it (and the scripts it names, in turn).
Anything it doesn't know affects all. Stdlib only; runs on Python 3.8+.
"""

import os
import re
import sys

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

    return pred()


def ev(e, env, unknown):
    kind = e[0]
    if kind == "all":
        return all(ev(a, env, unknown) for a in e[1])
    if kind == "any":
        return any(ev(a, env, unknown) for a in e[1])
    if kind == "not":
        return not ev(e[1][0], env, unknown)
    if kind == "kv":
        return env[e[1]] == e[2] if e[1] in env else unknown
    if kind == "name":
        return env[e[1]] if e[1] in env else unknown
    return unknown


def present(e, env, overrides=None):
    """Whether code under cfg(e) can be compiled for env (unknown predicates may be true)."""
    env = dict(env, **(overrides or {}))
    return ev(e, env, True)


def differs(e):
    return any(ev(e, MAC, u) != ev(e, LINUX, u) for u in (True, False))


def mac_only(e):
    """Code under cfg(e) is compiled on macOS and not on Linux, for some value of the unknowns."""
    return any(ev(e, MAC, u) and not ev(e, LINUX, u) for u in (True, False))


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
            uses.append((line(start), "file", parse_cfg(body)))
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
                    out.append((dm.group(1), target))
        return out


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


def module_tree(root, env, overrides=None):
    """{file: compiled?} for every file reachable from root on env. A file is compiled when every
    gate on its way (mod-declaration cfgs, the files' own #![cfg]) holds there."""
    seen = {}

    def visit(path, on, is_dir_owner):
        prev = seen.get(path)
        if prev is True or (prev is False and not on):
            return
        mods, uses, _ = scan(path)
        if on:
            for _, kind, e in uses:
                if kind == "file" and not present(e, env, overrides):
                    on = False
        seen[path] = on or bool(prev)
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


def compiled(crate, env, kinds=("lib", "bin", "test", "example", "bench", "build"), overrides=None):
    """{file: compiled?} over the crate's roots of the given kinds."""
    out = {}
    for root, kind in roots(crate):
        if kind not in kinds:
            continue
        if kind == "build":
            out[root] = True
            continue
        for f, on in module_tree(root, env, overrides).items():
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
    for e in errors:
        print("ci-scope: " + e, file=sys.stderr)
    if not errors:
        print("ci-scope: %s matches the %d crates that can behave differently on macOS" % (LIST, sum(1 for v in derived.values() if v)))
    return 1 if errors else 0


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
                    names.add(m.group(1))
    out = set()
    for n in names:
        out |= closure(crates, n)
    return out


REFERENCED = re.compile(r'"(?:\.\./)*((?:docs|scripts)/[A-Za-z0-9_./-]+)"')

# Paths no job here reads: docs, other workflows, local tooling, and scripts nothing below names.
NONE = re.compile(r"^(docs/|\.github/workflows/software-mode\.yml$|\.githooks/|\.local/|LICENSE$|\.gitignore$"
                  r"|Makefile$|bacon\.toml$|rustfmt\.toml$)|\.md$|^scripts/")
EVERYTHING = re.compile(r"^(\.github/workflows/ci\.yml|\.github/actions/|\.cargo/|rust-toolchain\.toml$|Cargo\.toml$"
                        r"|scripts/ci-scope\.py$)")
# What runs scripts in these jobs: the workflow and its actions (every job), and cargo's config
# (its runner, scripts/sign-run.sh, runs the macOS tests and the desktop's).
SCRIPT_USERS = ((".github/workflows/ci.yml", {"lint", "desktop", "emu"}), (".github/actions", {"lint", "desktop", "emu"}),
                (".cargo/config.toml", {"lint", "desktop"}))


def script_scopes(rust_refs):
    """{scripts/<file>: jobs} for the scripts a job runs: those the workflow, its actions or
    cargo's config name, those the tests name (rust_refs), and those a used script names in turn
    (sign-run.sh's hypervisor.entitlements)."""
    scripts = [f for f in walk_files("scripts") if os.path.isfile(os.path.join(ROOT, f))]
    named = lambda text, f: re.search(r"(?<![\w.-])%s(?![\w.-])" % re.escape(os.path.basename(f)), text)
    out = {}
    for user, jobs in SCRIPT_USERS:
        texts = [read(f) for f in (walk_files(user) if os.path.isdir(os.path.join(ROOT, user)) else [user])
                 if os.path.isfile(os.path.join(ROOT, f))]
        for f in scripts:
            if any(re.search(r"(?<![\w.-])%s(?![\w.-])" % re.escape(f), t) for t in texts):
                out.setdefault(f, set()).update(jobs)
    for f in rust_refs:
        if f in scripts:
            out.setdefault(f, set()).add("lint")
    grew = True
    while grew:
        grew = False
        for user in sorted(out):
            text = read(user)
            if not re.match(r"#!\s*/\S*(sh|bash)\b|#!\s*/usr/bin/env\s+(sh|bash)\b", text):
                continue  # only shell scripts run what they name; this one merely mentions some
            for f in scripts:
                if f != user and named(text, f) and not out[user] <= out.get(f, set()):
                    out.setdefault(f, set()).update(out[user])
                    grew = True
    return out


def scopes(paths):
    crates = workspace()
    by_dir = sorted(((c.dir + "/", n) for n, c in crates.items()), key=lambda t: -len(t[0]))
    emu = closure(crates, "emu")
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
    used_scripts = script_scopes(refs)
    need = {"lint": [], "desktop": [], "emu": []}
    for p in paths:
        p = p.strip()
        if not p:
            continue
        hit = set()
        owner = next((n for d, n in by_dir if p.startswith(d)), None)
        if p in refs:
            hit.add("lint")
        if EVERYTHING.match(p):
            hit |= {"lint", "desktop", "emu"}
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
                # A Rust file no module tree reaches here (deleted, or new and not yet declared)
                # counts; one the trees know is in a build only if a build compiles it.
                unknown = not (p.endswith(".rs") and p in on_mac)
                if owner in emu and not outside_build and release.get(p, unknown):
                    hit.add("emu")
                if owner in desk and not outside_build and desk_build.get(p, unknown):
                    hit.add("desktop")
        elif not NONE.search(p):
            hit |= {"lint", "desktop", "emu"}
        for k in hit:
            need[k].append(p)
    return need


def cmd_changes(args):
    if "--all" in args:
        need = {"lint": ["(all)"], "desktop": ["(all)"], "emu": ["(all)"]}
    else:
        need = scopes(sys.stdin.read().splitlines())
    for k in ("lint", "desktop", "emu"):
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

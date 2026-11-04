#!/usr/bin/env python3
"""The web build's rewrite of upstream code (web/README.md, "Merging upstream").

std's file system, clocks, environment, threads and locks fail, panic or block in a browser, so the
crates the web build compiles reach them through horizon_platform, which is std natively. Upstream
writes `std::fs`, `std::time`, `std::thread`, `std::env` and `std::sync::{Mutex, RwLock}`; this
routes them through horizon_platform, in place:

    python3 tools/web_rewrite.py FILE...                 # these files
    python3 tools/web_rewrite.py --changed A B           # files of the web crates B changed since A

`--changed` takes the crates that depend on horizon-platform (their Cargo.toml says so), less
horizon-platform and horizon-threads, and their `src/` files that changed between the two revisions.
Before merging upstream, what the merge brings is `--changed $(git merge-base HEAD upstream/main)
upstream/main`. Rewritten files are run through rustfmt. What it can't decide stays for a person: a `horizon_platform::thread::Builder` spawn runs
in place on the web (a loop there hangs the thread that started it), so loops go through
`spawn_thread` / `spawn_worker` by hand; tools/upstream_watch.py lists the new ones.
"""
import argparse
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
PLATFORM = {"fs", "time", "thread", "env"}
LOCKS = {"Mutex", "RwLock"}
# horizon-ctl talks to a running game natively; it is never built for the web.
SKIP = ("bin/horizon-ctl",)


# --- std::{fs, time, thread, env} -----------------------------------------------------------

def split_top(s):
    items, depth, cur = [], 0, ""
    for ch in s:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        if ch == "," and depth == 0:
            items.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        items.append(cur)
    return items


def head(item):
    t = re.sub(r"^(//[^\n]*\n\s*)+", "", item.strip())
    return re.split(r"[:\s{]", t, maxsplit=1)[0]


def rewrite_platform_uses(src):
    out, i = [], 0
    pat = re.compile(r"(?m)^(?P<ind>[ \t]*)(?P<vis>pub(\([^)]*\))? )?use std::\{")
    while True:
        m = pat.search(src, i)
        if not m:
            out.append(src[i:])
            break
        out.append(src[i:m.start()])
        j, depth = m.end(), 1
        while depth:
            if src[j] == "{":
                depth += 1
            elif src[j] == "}":
                depth -= 1
            j += 1
        inner, end = src[m.end():j - 1], j
        if src[end] == ";":
            end += 1
        items = split_top(inner)
        keep = [x for x in items if head(x) not in PLATFORM]
        moved = [x for x in items if head(x) in PLATFORM]
        ind, vis = m.group("ind"), m.group("vis") or ""
        if not moved:
            out.append(src[m.start():end])
        else:
            parts = []
            if keep:
                parts.append(f"{ind}{vis}use std::{{{', '.join(x.strip() for x in keep)}}};")
            parts.append(f"{ind}{vis}use horizon_platform::{{{', '.join(x.strip() for x in moved)}}};")
            out.append("\n".join(parts))
        i = end
    src = "".join(out)
    return re.sub(r"\bstd::(fs|time|thread|env)\b(?!!)", r"horizon_platform::\1", src)


# --- std::sync::{Mutex, RwLock} ---------------------------------------------------------------

def parse_tree(s, i):
    """A use tree at s[i:]: its (segments, alias) paths, and the index after it."""
    def skip(i):
        while i < len(s) and s[i] in " \t\r\n":
            i += 1
        while s.startswith("//", i):
            i = s.index("\n", i) + 1
            while i < len(s) and s[i] in " \t\r\n":
                i += 1
        return i

    segs = []
    while True:
        i = skip(i)
        if s[i] == "{":
            i += 1
            paths = []
            while True:
                i = skip(i)
                if s[i] == "}":
                    return paths, i + 1
                sub, i = parse_tree(s, i)
                paths += [(segs + p, a) for p, a in sub]
                i = skip(i)
                if s[i] == ",":
                    i += 1
        if s[i] == "*":
            return [(segs + ["*"], None)], i + 1
        m = re.compile(r"[A-Za-z_][A-Za-z0-9_]*").match(s, i)
        segs.append(m.group(0))
        i = skip(m.end())
        if s.startswith("::", i):
            i += 2
            continue
        alias = None
        m2 = re.compile(r"as\s+([A-Za-z_][A-Za-z0-9_]*)").match(s, i)
        if m2:
            alias, i = m2.group(1), m2.end()
        return [(segs, alias)], i


def emit(paths):
    def build(items):
        order, groups = [], {}
        for segs, alias in items:
            if segs[0] not in groups:
                groups[segs[0]] = []
                order.append(segs[0])
            groups[segs[0]].append((segs[1:], alias))
        out = []
        for h in order:
            rest = groups[h]
            for _, alias in [(r, a) for r, a in rest if not r]:
                out.append(h + (f" as {alias}" if alias else ""))
            deeper = [(r, a) for r, a in rest if r]
            if deeper:
                inner = build(deeper)
                out.append(f"{h}::{inner[0]}" if len(inner) == 1 else f"{h}::{{{', '.join(inner)}}}")
        return out

    inner = build(paths)
    return inner[0] if len(inner) == 1 else "{" + ", ".join(inner) + "}"


USE = re.compile(r"(?m)^(?P<ind>[ \t]*)(?P<vis>pub(\([^)]*\))? )?use (?P<root>::)?std::")


def rewrite_locks(src):
    out, i = [], 0
    while True:
        m = USE.search(src, i)
        if not m:
            out.append(src[i:])
            break
        paths, end = parse_tree(src, m.end() - len("std::"))
        semi = src.index(";", end) + 1
        moved = [(p, a) for p, a in paths if len(p) == 3 and p[1] == "sync" and p[2] in LOCKS]
        if not moved:
            out.append(src[i:semi])
            i = semi
            continue
        kept = [(p, a) for p, a in paths if (p, a) not in moved]
        ind, vis = m.group("ind"), m.group("vis") or ""
        before = src[i:m.start()]
        lines, attrs = before.split("\n"), []
        k = len(lines) - 2
        while k >= 0 and lines[k].strip().startswith("#["):
            attrs.insert(0, lines[k])
            k -= 1
        out.append(before)
        names = [p[2] + (f" as {a}" if a else "") for p, a in moved]
        names = names[0] if len(names) == 1 else "{" + ", ".join(names) + "}"
        platform = f"{ind}{vis}use horizon_platform::sync::{names};"
        if kept:
            prefix = "\n".join(attrs) + "\n" if attrs else ""
            out.append(f"{ind}{vis}use {emit(kept)};\n{prefix}{platform}")
        else:
            out.append(platform)
        i = semi
    return re.sub(r"\bstd::sync::(Mutex|RwLock)\b", r"horizon_platform::sync::\1", "".join(out))


def rewrite(src):
    return rewrite_locks(rewrite_platform_uses(src))


# --- which files -------------------------------------------------------------------------------

def web_crates():
    """The crate folders whose sources go through horizon_platform."""
    out = []
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        name = manifest.parent.name
        if name in ("horizon-platform", "horizon-threads"):
            continue
        if re.search(r"^horizon-platform\s*=", manifest.read_text(), re.M):
            out.append(manifest.parent)
    return out


def changed_files(base, head_rev):
    dirs = [str((c / "src").relative_to(ROOT)) for c in web_crates()]
    names = subprocess.run(
        ["git", "diff", "--name-only", "--diff-filter=AM", base, head_rev, "--", *dirs],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout.split()
    return [ROOT / n for n in names if n.endswith(".rs")]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("files", nargs="*", type=pathlib.Path)
    ap.add_argument("--changed", nargs=2, metavar=("BASE", "HEAD"),
                    help="the web crates' files HEAD changed since BASE")
    args = ap.parse_args()
    files = [p.resolve() for p in args.files]
    if args.changed:
        files += changed_files(*args.changed)
    done = []
    for p in files:
        if not p.is_file() or any(s in str(p) for s in SKIP):
            continue
        s = p.read_text()
        n = rewrite(s)
        if n != s:
            p.write_text(n)
            done.append(p)
    if done:
        subprocess.run(["rustfmt", "--edition", "2024", *map(str, done)], cwd=ROOT)
    for p in done:
        print(p.relative_to(ROOT))
    print(f"web_rewrite: {len(done)} of {len(files)} files rewritten", file=sys.stderr)


if __name__ == "__main__":
    main()

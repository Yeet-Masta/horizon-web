#!/usr/bin/env python3
"""What upstream code needs a person before the web build takes it (web/README.md, "Merging upstream").

The std calls the web build can't make are routed by tools/web_rewrite.py and caught by
tools/web_hazards.py. What neither can decide is listed here, from the lines a range of upstream
commits (or an open pull request) adds to the crates the web build compiles:

- spawns through `thread::Builder`: horizon_platform's runs in place on the web, so a loop there
  hangs the thread that started it (`spawn_thread` / `spawn_worker` start a Web Worker);
- `pollster::block_on`: on the page's thread it waits with Atomics.wait, which a browser refuses
  (`horizon_platform::block_on_or` doesn't wait there);
- `ureq`, `std::net`, `mio`: no sockets or HTTP client of the game's own in a browser;
- chrono's or std's clocks in a crate that doesn't go through horizon_platform;
- new crates in Cargo.lock, which may not build for wasm32 or wasm64.

    python3 tools/upstream_watch.py --base A --head B           # a merge's range, as Markdown
    python3 tools/upstream_watch.py --prs 9468 9467 ...         # open pull requests (their heads
                                                                # fetched from the upstream remote)
    python3 tools/upstream_watch.py --open-prs ludovic111/horizon  # every open one (gh api)

Tests, examples and benches are left out: they never run in a browser.
"""
import argparse
import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

CHECKS = [
    ("spawn", re.compile(r"thread::Builder::new\(\)|std::thread::spawn\(|thread::spawn\("),
     "a thread spawn: a loop must go through spawn_thread / spawn_worker on the web"),
    ("block_on", re.compile(r"pollster::block_on\("),
     "pollster::block_on: blocks the page (Atomics.wait); horizon_platform::block_on_or doesn't"),
    ("network", re.compile(r"\bureq::|std::net::(TcpStream|TcpListener|UdpSocket)|\bmio::"),
     "a socket or HTTP client the browser build doesn't have"),
    ("clock", re.compile(r"std::time::(Instant|SystemTime)::now|chrono::(Utc|Local)::now"),
     "a clock std or chrono reads natively; horizon_platform::time's works on the web"),
    ("wait", re.compile(r"\.recv\(\)|\.wait\(|wait_timeout|wait_while|park\(\)"),
     "a blocking wait: fine on a worker, an Atomics.wait trap on the page's thread"),
]
TEST_PATH = re.compile(r"(^|/)(tests?|examples|benches)/|_tests?\.rs$|/tests\.rs$|build\.rs$")


def git(*args, check=True):
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True,
                          check=check).stdout


def web_dirs(cache=[]):
    """The `src/` folders of the crates the web build links: horizon-client's dependencies on
    wasm32 without default features, as web/build.sh builds it (every crate when cargo can't say)."""
    if cache:
        return cache
    meta = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--filter-platform", "wasm32-unknown-unknown",
         "--no-default-features"], cwd=ROOT, capture_output=True, text=True)
    dirs = []
    if meta.returncode == 0:
        data = json.loads(meta.stdout)
        packages = {p["id"]: p for p in data["packages"]}
        nodes = {n["id"]: n for n in data["resolve"]["nodes"]}
        start = next(p["id"] for p in data["packages"] if p["name"] == "horizon-client")
        seen, todo = set(), [start]
        while todo:
            node = todo.pop()
            if node in seen:
                continue
            seen.add(node)
            todo += [d["pkg"] for d in nodes[node]["deps"]
                     if any(k["kind"] is None for k in d["dep_kinds"])]
        for node in seen:
            folder = pathlib.Path(packages[node]["manifest_path"]).parent
            if folder.is_relative_to(ROOT / "crates") and folder.name != "horizon-platform":
                dirs.append(str(folder.relative_to(ROOT) / "src"))
    if not dirs:
        dirs = [str(m.parent.relative_to(ROOT) / "src")
                for m in sorted((ROOT / "crates").glob("*/Cargo.toml"))]
    cache.extend(sorted(dirs))
    return cache


def added_lines(base, head):
    """(file, line number, text) of every line `head` adds over `base` in the crates' sources."""
    diff = git("diff", "--unified=0", "--no-color", base, head, "--", *web_dirs())
    out, path, line = [], None, 0
    for raw in diff.splitlines():
        if raw.startswith("+++ "):
            path = raw[6:] if raw.startswith("+++ b/") else None
        elif raw.startswith("@@"):
            m = re.search(r"\+(\d+)", raw)
            line = int(m.group(1)) if m else 0
        elif raw.startswith("+") and path and not TEST_PATH.search(path):
            out.append((path, line, raw[1:]))
            line += 1
    return out


def in_test_module(path, rev, line, cache={}):
    """Whether `line` of `path` at `rev` is past the file's first `#[cfg(test)]`: a test module."""
    key = (path, rev)
    if key not in cache:
        text = git("show", f"{rev}:{path}", check=False)
        first = next((i + 1 for i, l in enumerate(text.splitlines())
                      if l.startswith("#[cfg(test)]") or l.startswith("mod tests")), None)
        cache[key] = first
    first = cache[key]
    return first is not None and line > first


def findings(base, head):
    found = {name: [] for name, _, _ in CHECKS}
    for path, line, text in added_lines(base, head):
        code = text.split("//")[0]
        for name, pattern, _ in CHECKS:
            if pattern.search(code) and not in_test_module(path, head, line):
                found[name].append((path, line, text.strip()))
    return found


def new_crates(base, head):
    def crates(rev):
        lock = git("show", f"{rev}:Cargo.lock", check=False)
        return set(re.findall(r'\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', lock))
    return sorted(crates(head) - crates(base))


def report_range(base, head, title):
    found = findings(base, head)
    lines = [f"### {title}", ""]
    total = sum(len(v) for v in found.values())
    if not total:
        lines.append("Nothing for the web build to look at.")
    for name, _, why in CHECKS:
        hits = found[name]
        if not hits:
            continue
        lines.append(f"**{name}** ({len(hits)}): {why}")
        for path, line, text in hits[:40]:
            lines.append(f"- `{path}:{line}` `{text[:110]}`")
        if len(hits) > 40:
            lines.append(f"- ... and {len(hits) - 40} more")
        lines.append("")
    crates = new_crates(base, head)
    if crates:
        lines.append(f"**new crates** ({len(crates)}): " + ", ".join(f"{n} {v}" for n, v in crates[:60]))
    return "\n".join(lines), total


def pr_heads(numbers, remote):
    refspecs = [f"+refs/pull/{n}/head:refs/upstream-pr/{n}" for n in numbers]
    for i in range(0, len(refspecs), 100):
        git("fetch", "-q", "--no-tags", remote, *refspecs[i:i + 100], check=False)
    return {n: git("rev-parse", "-q", "--verify", f"refs/upstream-pr/{n}", check=False).strip()
            for n in numbers}


def open_prs(repo):
    out = subprocess.run(["gh", "api", "--paginate", f"repos/{repo}/pulls?state=open&per_page=100",
                          "--jq", ".[] | [.number, .title, .user.login] | @tsv"],
                         capture_output=True, text=True)
    if out.returncode != 0:
        print(f"upstream_watch: the open pull requests of {repo}: {out.stderr.strip()}",
              file=sys.stderr)
        return []
    prs = []
    for row in out.stdout.splitlines():
        n, title, login = row.split("\t", 2)
        prs.append((int(n), title, login))
    return prs


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--base")
    ap.add_argument("--head")
    ap.add_argument("--prs", nargs="*", type=int, default=[])
    ap.add_argument("--open-prs", metavar="OWNER/REPO")
    ap.add_argument("--remote", default="upstream", help="where pull request heads are fetched")
    ap.add_argument("--main", default="upstream/main", help="what the pull requests go into")
    ap.add_argument("--json", type=pathlib.Path, help="also write the counts here")
    args = ap.parse_args()
    parts, counts = [], {}
    if args.base and args.head:
        short = lambda rev: git("rev-parse", "--short", rev, check=False).strip() or rev
        text, total = report_range(args.base, args.head,
                                   f"Upstream {short(args.base)}..{short(args.head)}")
        parts.append(text)
        counts["range"] = total
    titles = {}
    numbers = list(args.prs)
    if args.open_prs:
        for n, title, login in open_prs(args.open_prs):
            titles[n] = f"{title} ({login})"
            numbers.append(n)
    if numbers:
        watched = []
        for n, head in sorted(pr_heads(sorted(set(numbers)), args.remote).items(), reverse=True):
            if not head:
                continue
            base = git("merge-base", head, args.main, check=False).strip()
            if not base:
                continue
            found = findings(base, head)
            total = sum(len(v) for v in found.values())
            if total:
                kinds = ", ".join(f"{k} {len(v)}" for k, v in found.items() if v)
                watched.append((n, kinds, found))
        counts["prs"] = len(watched)
        lines = ["### Open upstream pull requests the web build will need to look at", ""]
        if not watched:
            lines.append("None.")
        for n, kinds, found in watched:
            lines.append(f"- #{n} {titles.get(n, '')}: {kinds}")
            for name, hits in found.items():
                for path, line, text in hits[:3]:
                    lines.append(f"  - {name}: `{path}:{line}` `{text[:90]}`")
        parts.append("\n".join(lines))
    print("\n\n".join(parts))
    if args.json:
        args.json.write_text(json.dumps(counts))


if __name__ == "__main__":
    main()

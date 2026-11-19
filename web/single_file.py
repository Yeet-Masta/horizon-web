#!/usr/bin/env python3
"""Folds a one-thread web build (web/dist) into one HTML file: web/build.sh --single-file.

    python3 web/single_file.py [DIST] [OUT]     # default: web/dist, DIST/horizon.html

The page opens from anywhere, a file on disk included: no server and no headers. Only the
one-thread build: threads need cross-origin isolation, which a server's headers give and a
file can't. The module is stored gzipped (the browser's DecompressionStream inflates it), its
JS glue and the page's script inline, the fonts and the icon as data URLs. The resources zip
is still the player's own: drop it on the page, as anywhere else. The one-thread build can't
host single player yet (web/README.md).
"""
import base64
import gzip
import json
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent

# Answers the page's fetches of the files folded in (build.json, the module), and lets every
# other one through. The module's bytes are base64 in an inert <script> (no JS to parse).
BOOT = """
(() => {
  const files = %(files)s;
  const real = globalThis.fetch.bind(globalThis);
  const bytes = async (id) => {
    const text = document.getElementById(id).textContent.trim();
    return (await real("data:application/octet-stream;base64," + text)).blob();
  };
  globalThis.fetch = async (input, init) => {
    const url = new URL(input instanceof Request ? input.url : String(input), document.baseURI);
    const name = Object.keys(files).find((n) => url.pathname.endsWith("/" + n));
    if (!name || (init?.method ?? "GET").toUpperCase() !== "GET") return real(input, init);
    const { id, type, gzip } = files[name];
    let body = await bytes(id);
    if (gzip) body = body.stream().pipeThrough(new DecompressionStream("gzip"));
    return new Response(body, { headers: { "Content-Type": type } });
  };
})();
"""


def data_url(path, mime):
    return f"data:{mime};base64,{base64.b64encode(path.read_bytes()).decode()}"


def inline_script(text):
    # A script's text ends at the first "</script": none may appear inside.
    return re.sub(r"</(script)", r"<\\/\1", text, flags=re.IGNORECASE)


def main():
    dist = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "dist"
    out = pathlib.Path(sys.argv[2]) if len(sys.argv) > 2 else dist / "horizon.html"
    build = json.loads((dist / "build.json").read_text(encoding="utf-8"))
    if build.get("threads"):
        sys.exit("single_file: the threaded build needs a server's headers: build with --no-threads")
    page = (dist / "index.html").read_text(encoding="utf-8")
    page = page.replace('href="favicon.svg"', f'href="{data_url(dist / "favicon.svg", "image/svg+xml")}"')
    for font in (dist / "fonts").glob("*.ttf"):
        page = page.replace(f'url("fonts/{font.name}")', f'url("{data_url(font, "font/ttf")}")')

    wasm = gzip.compress((dist / "pkg" / "horizon-client_bg.wasm").read_bytes(), 9, mtime=0)
    files = {
        "build.json": {"id": "horizon-build", "type": "application/json", "gzip": False},
        "pkg/horizon-client_bg.wasm": {"id": "horizon-module", "type": "application/wasm", "gzip": True},
    }
    glue = data_url(dist / "pkg" / "horizon-client.js", "text/javascript")
    head = "\n".join([
        f'<script type="application/octet-stream" id="horizon-build">'
        f'{base64.b64encode((dist / "build.json").read_bytes()).decode()}</script>',
        f'<script type="application/octet-stream" id="horizon-module">{base64.b64encode(wasm).decode()}</script>',
        f"<script>{BOOT % {'files': json.dumps(files)}}</script>",
        # horizon.js imports the glue by this name.
        '<script type="importmap">' + json.dumps({"imports": {"./pkg/horizon-client.js": glue}}) + "</script>",
    ])
    page = page.replace("</head>", head + "\n</head>", 1)
    script = '<script type="module" src="horizon.js"></script>'
    if script not in page:
        sys.exit("single_file: index.html no longer loads horizon.js as expected")
    horizon = (dist / "horizon.js").read_text(encoding="utf-8")
    page = page.replace(script, f'<script type="module">\n{inline_script(horizon)}\n</script>', 1)
    for left in ("favicon.svg", 'url("fonts/'):
        if left in page:
            sys.exit(f"single_file: {left} is still loaded by URL")
    out.write_text(page, encoding="utf-8")
    print(f"{out}: {out.stat().st_size / 1e6:.0f} MB, one file")


if __name__ == "__main__":
    main()

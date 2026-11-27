#!/usr/bin/env bash
# Zips your prepared resources for the web build (web/README.md), into the gitignored
# .local/horizon-web-resources.zip that web/serve.py hands to the page on this machine.
#
#   web/pack-resources.sh [--no-sounds] [RESOURCES_DIR]
#
# --no-sounds leaves the sounds out: most of the size, and the game runs silent without them.
#
# RESOURCES_DIR defaults to .local/horizon-client/<version> (what `horizon-prepare --prepare`
# writes); a release folder's `resources/` works too. The zip holds files derived from your
# own copy of Minecraft: keep it to yourself, never upload it anywhere public.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(grep -m1 '"version"' "$root/research-lock.json" | sed 's/.*: *"\([^"]*\)".*/\1/')"
sounds=1
if [ "${1:-}" = --no-sounds ]; then
  sounds=0
  shift
fi
src="${1:-$root/.local/horizon-client/$version}"
out="$root/.local/horizon-web-resources.zip"
if [ ! -f "$src/horizon-resources.json" ]; then
  echo "no prepared resources in $src (horizon-resources.json is missing)." >&2
  echo "Prepare them first: cargo run --release -p horizon-prepare -- --prepare --server-jar PATH" >&2
  exit 1
fi
mkdir -p "$(dirname "$out")"
if [ "$sounds" = 1 ] && [ -z "$(find "$src/assets" -path '*/sounds/*.ogg' -print -quit 2>/dev/null)" ]; then
  echo "no sounds in $src: the game runs silent (horizon-prepare sounds copies them first)" >&2
fi
# The sounds last, so the page can keep the rest in memory and read each sound from the zip
# when it first plays (vfs.rs, mount_zip_blob_eager). Ogg Vorbis already: stored, not compressed
# again. Without Info-ZIP's zip (Git Bash on Windows has none), Python's zipfile writes the same.
if command -v zip >/dev/null; then
  rm -f "$out"
  (cd "$src" && zip -qr -1 -n .png "$out" . -x 'assets/*/sounds/*.ogg')
  if [ "$sounds" = 1 ]; then
    (cd "$src" && find assets -path 'assets/*/sounds/*.ogg' -print | zip -q -0 "$out" -@)
  fi
else
  python="$(command -v python3 || command -v python)" || {
    echo "neither zip nor Python is on the PATH" >&2
    exit 1
  }
  "$python" - "$src" "$out" "$sounds" <<'PY'
import fnmatch, os, sys, zipfile

src, out, sounds = sys.argv[1], sys.argv[2], sys.argv[3] == "1"
files = sorted(
    os.path.relpath(os.path.join(d, f), src).replace(os.sep, "/")
    for d, _, names in os.walk(src)
    for f in names
)
sound = lambda path: fnmatch.fnmatch(path, "assets/*/sounds/*.ogg")
with zipfile.ZipFile(out + ".part", "w") as z:
    for path in (f for f in files if not sound(f)):
        if path.endswith(".png"):
            z.write(os.path.join(src, path), path, zipfile.ZIP_STORED)
        else:
            z.write(os.path.join(src, path), path, zipfile.ZIP_DEFLATED, 1)
    for path in (f for f in files if sounds and sound(f)):
        z.write(os.path.join(src, path), path, zipfile.ZIP_STORED)
os.replace(out + ".part", out)
PY
fi
echo "$out: $(du -h "$out" | cut -f1)"

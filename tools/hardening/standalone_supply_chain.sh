#!/usr/bin/env bash
# Supply-chain evidence for ONE publishable crate, as a crates.io consumer sees it
# (use-protection-please H-08 / H-09 / H-10 / H-12).
#
#   tools/hardening/standalone_supply_chain.sh crates/rusty_mp3 [out-dir]
#
# The workspace lock spans ~770 crates across every codec, the TLS stack and the
# UI, so `cargo audit` / `cargo vet` at the root answer for the whole repo, not for
# one crate. This copies the crate out of the workspace, pins its `.workspace =
# true` dependencies to the workspace's versions, gives it its own lockfile, and
# runs the four checks against exactly the closure a downstream user receives:
#
#   cargo audit --deny warnings          (H-09)
#   cargo deny check  (repo deny.toml)   (H-08)
#   cargo vet  (store: <crate>/supply-chain, committed)   (H-10)
#   cargo cyclonedx -> <out>/<crate>.cdx.json             (H-12)
#
# Exits non-zero on the first failing check.
set -euo pipefail
crate_dir="$(cd "$1" && pwd)"
root="$(cd "$(dirname "$0")/../.." && pwd)"
name="$(basename "$crate_dir")"
out="${2:-$root/target/hardening/$name}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Copy the crate's source tree (not target/, fuzz/, or lab output).
( cd "$crate_dir" && tar cf - --exclude=./target --exclude=./fuzz --exclude=./lab-results . ) | ( cd "$work" && tar xf - )

# Resolve `foo.workspace = true` against the root manifest's [workspace.dependencies].
PY="$(command -v python3 || command -v python)"
"$PY" - "$root/Cargo.toml" "$work/Cargo.toml" <<'PY'
import re, sys
root = open(sys.argv[1], encoding="utf-8-sig").read()
def table(name):
    if f"[{name}]" not in root:
        return {}
    body = root.split(f"[{name}]")[1].split("\n[")[0]
    return {m.group(1): m.group(2).strip()
            for m in re.finditer(r'^([A-Za-z0-9_-]+)\s*=\s*(.+?)\s*$', body, re.M)}
deps, pkg = table("workspace.dependencies"), table("workspace.package")
man = open(sys.argv[2], encoding="utf-8").read()
head, sep, rest = man.partition("\n[dependencies]")
# [package] fields inherited from [workspace.package] (an edition dropped here would
# silently fall back to 2015).
head = re.sub(r'^([A-Za-z0-9_-]+)\.workspace\s*=\s*true\s*$',
              lambda m: f"{m.group(1)} = {pkg[m.group(1)]}", head, flags=re.M)
def sub(m):
    spec = deps[m.group(1)]
    if spec.startswith('"'):
        return f'{m.group(1)} = {spec}'          # a plain version string: verbatim
    v = re.search(r'version\s*=\s*"([^"]+)"', spec)
    if not v:
        return m.group(0)
    ver = v.group(1)
    return f'{m.group(1)} = "{ver if ver.startswith("=") else "=" + ver}"'
rest = re.sub(r'^([A-Za-z0-9_-]+)\.workspace\s*=\s*true\s*$', sub, rest, flags=re.M)
open(sys.argv[2], "w", encoding="utf-8").write(head + sep + rest + "\n[workspace]\n")
PY

cp "$root/deny.toml" "$work/"
cd "$work"
cargo generate-lockfile -q
echo "== closure"; cargo tree -e normal,dev --prefix none | sort -u
echo "== cargo audit (H-09)"; cargo audit --deny warnings -q && echo "audit: clean"
echo "== cargo deny (H-08)"; cargo deny --color never check 2>&1 | tail -6   # exit status is cargo-deny's (pipefail); never grep a verdict
echo "== cargo vet (H-10)"
if [ -n "${VET_UPDATE:-}" ]; then
  # First run / store maintenance: fetch imports, then write the store back to the crate.
  cargo vet 2>&1 | tail -40 || true
  cp supply-chain/*.toml supply-chain/imports.lock "$crate_dir/supply-chain/" 2>/dev/null || true
fi
cargo vet --locked 2>&1 | tail -2
echo "== SBOM (H-12)"; mkdir -p "$out"
cargo cyclonedx -q -f json --spec-version 1.5 --no-build-deps --override-filename "$name.cdx"
cp "$name.cdx.json" "$out/" && echo "sbom: $out/$name.cdx.json"

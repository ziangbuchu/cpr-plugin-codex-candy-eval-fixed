#!/usr/bin/env bash
# Build and package through CPR's official CLI; never overwrite an existing release.
set -euo pipefail
umask 077
root="$(cd "$(dirname "$0")/.." && pwd)"
cargo="${CARGO:-${HOME}/.cargo/bin/cargo}"
cli="${CPR_PLUGIN_CLI:-${HOME}/.cargo/bin/cpr-plugin}"
[[ -x "$cargo" ]] || cargo="$(command -v cargo)"
[[ -x "$cli" ]] || cli="$(command -v cpr-plugin)"
version="$(python3 - "$root/plugin.json" <<'PY'
import json,re,sys
value=json.load(open(sys.argv[1]))['version']
assert re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?',value)
print(value)
PY
)"
output="$root/dist/native-$version"
# Keep earlier candidates intact. A version can be packaged only into a new directory.
[[ ! -e "$output" ]] || { printf '%s\n' 'Release output already exists; refusing to overwrite.' >&2; exit 1; }
"$cargo" fmt --manifest-path "$root/Cargo.toml" -- --check
"$cargo" test --offline --locked --manifest-path "$root/Cargo.toml"
node "$root/tests/check-page.mjs" "$root"
node "$root/tools/build-page.mjs" "$root"
"$cargo" build --offline --locked --release --manifest-path "$root/Cargo.toml"
"$cli" package \
  --manifest "$root/plugin.json" \
  --binary "${CARGO_TARGET_DIR:-$root/target}/release/codex-proxy-plugin-candy-eval" \
  --target x86_64-unknown-linux-gnu \
  --resource-map "web=build/resources-$version/web" \
  --output-dir "$output"
# Independently verify the official per-file digests and forbid external script resources.
python3 - "$output" "$version" <<'PY'
import hashlib,json,sys,tarfile
from pathlib import Path
folder=Path(sys.argv[1]); version=sys.argv[2]
package=folder/f'nmbb.codex-candy-eval-{version}-x86_64-unknown-linux-gnu.tar.gz'
with tarfile.open(package) as archive:
    manifest=json.load(archive.extractfile('plugin.json'))
    assert manifest['version']==version
    assert manifest['package']['protocolVersion']==6
    assert set(archive.getnames())=={'plugin.json',*manifest['package']['files']}
    assert not any(name.endswith('.js') for name in manifest['resources'])
    for name,digest in manifest['package']['files'].items():
        assert hashlib.sha256(archive.extractfile(name).read()).hexdigest()==digest
actual=hashlib.sha256(package.read_bytes()).hexdigest()
assert package.with_name(package.name+'.sha256').read_text().split()[0]==actual
print(json.dumps({'version':version,'official_package':True,'file_digests_verified':True,'sha256':actual}))
PY

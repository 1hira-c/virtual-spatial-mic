"""Package the MPL-2.0 crate sources shipped in the VSM executable.

MPL-2.0 requires the Source Code Form of covered files to be available to
recipients of the executable. VSM uses these crates unmodified, so the archive
holds the exact registry .crate files, verified against Cargo.lock checksums.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tomllib
import zipfile

import rust_notices

ROOT = Path(__file__).resolve().parents[1]
STAMP = (1980, 1, 1, 0, 0, 0)


def cached_crate(name, version):
    home = Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo'))
    matches = sorted((home / 'registry' / 'cache').glob(f'*/{name}-{version}.crate'))
    if not matches:
        raise RuntimeError(f'{name} {version}.crate is not in the Cargo cache')
    return matches[0]


def add(archive, name, data):
    info = zipfile.ZipInfo(name, STAMP)
    info.compress_type = zipfile.ZIP_DEFLATED
    archive.writestr(info, data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        raise RuntimeError('Source archive destination already exists.')
    subprocess.run(['cargo', 'fetch', '--locked'], cwd=ROOT, check=True)
    lock = tomllib.loads((ROOT / 'Cargo.lock').read_text(encoding='utf-8'))
    checksums = {(p['name'], p['version']): p.get('checksum') for p in lock['package']}
    crates = [p for p in rust_notices.shipped_packages(rust_notices.metadata())
              if rust_notices.is_mpl(p['license'])]
    if not crates:
        raise RuntimeError('No MPL-covered crates found; update LICENSES.md instead of shipping an empty archive.')
    rows = []
    args.output.parent.mkdir(parents=True, exist_ok=True)
    partial = args.output.with_suffix(args.output.suffix + '.partial')
    with zipfile.ZipFile(partial, 'x') as archive:
        for package in crates:
            name, version = package['name'], package['version']
            data = cached_crate(name, version).read_bytes()
            digest = hashlib.sha256(data).hexdigest()
            if digest != checksums.get((name, version)):
                raise RuntimeError(f'{name} {version}.crate does not match Cargo.lock')
            add(archive, f'crates/{name}-{version}.crate', data)
            rows.append({'name': name, 'version': version, 'license': package['license'],
                         'repository': package.get('repository'), 'sha256': digest,
                         'download': f'https://static.crates.io/crates/{name}/{name}-{version}.crate'})
        add(archive, 'MPL-2.0.txt', (ROOT / 'docs/third-party/MPL-2.0.txt').read_bytes())
        add(archive, 'SOURCES.json', json.dumps(rows, indent=2) + '\n')
        add(archive, 'README.txt',
            'Virtual Spatial Mic: MPL-2.0 covered sources\n\n'
            'vsm.exe contains the crates listed in SOURCES.json, used without modification.\n'
            'Each crates/*.crate file is the original crates.io package (a gzip-compressed\n'
            'tar archive) with the SHA-256 recorded in Cargo.lock. The MPL-2.0 applies to\n'
            'these files only; it does not change the license of other VSM components.\n'
            'VSM source code and build instructions: see SOURCE.txt next to vsm.exe.\n')
    partial.rename(args.output)
    print('MPL sources:', args.output)


if __name__ == '__main__':
    main()

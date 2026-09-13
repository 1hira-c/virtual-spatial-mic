"""Download and verify the pinned Steam Audio SDK for development builds."""
import hashlib
import json
from pathlib import Path
import shutil
import sys
import urllib.request
import zipfile


def main():
    root = Path(__file__).resolve().parents[1]
    lock = json.loads((root / 'tools/dependencies.lock.json').read_text(encoding='utf-8'))
    item = lock['files'][0]
    cache = root / '.deps/downloads' / item['file']
    cache.parent.mkdir(parents=True, exist_ok=True)
    if not cache.exists():
        partial = cache.with_suffix('.partial')
        with urllib.request.urlopen(item['url'], timeout=120) as response, partial.open('wb') as output:
            shutil.copyfileobj(response, output)
        if hashlib.sha256(partial.read_bytes()).hexdigest() != item['sha256']:
            raise RuntimeError('Downloaded SDK hash mismatch')
        partial.rename(cache)
    if hashlib.sha256(cache.read_bytes()).hexdigest() != item['sha256']:
        raise RuntimeError('Cached SDK hash mismatch')
    if sys.platform == 'win32':
        runtime = ['steamaudio/lib/windows-x64/phonon.dll']
    elif sys.platform.startswith('linux'):
        runtime = ['steamaudio/lib/linux-x64/libphonon.so']
    else:
        raise RuntimeError('SDK setup currently supports Windows x64 and Linux x86-64')
    dest = (root / '.deps/steam-audio-4.8.1').resolve()
    with zipfile.ZipFile(cache) as archive:
        # License texts are already retained in docs/third-party; the SDK zip
        # does not contain LICENSE.md under the runtime directory.
        for name in runtime:
            target = dest / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(archive.read(name))
    print('Steam Audio verified and ready. See docs/development.md for build commands.')


if __name__ == '__main__':
    main()

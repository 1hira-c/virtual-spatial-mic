"""Package corresponding sources from a clean public VSM checkout."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import zipfile

import build_steam_audio as steam

ROOT = Path(__file__).resolve().parents[1]


def git(*args, cwd=ROOT):
    return subprocess.check_output(['git', '-c', 'safe.directory=' + cwd.as_posix(), '-C', str(cwd), *args])


def source_files(repo):
    marker = repo / '.vsm-source.json'
    if marker.is_file():
        return json.loads(marker.read_text())['files']
    entries = git('ls-files', '--stage', '-z', cwd=repo).decode().strip('\0').split('\0')
    # PFFFT's disabled benchmark backends are optional submodules, not linked
    # dependencies. Keep all regular tracked sources and their license files.
    return [entry.split('\t', 1)[1] for entry in entries if not entry.startswith('160000 ')]


def read_source(repo, name):
    file = repo / name
    if not file.is_file() and repo.name == 'zlib' and name == 'zconf.h':
        # zlib's own CMake configuration renames this tracked input to .in.
        file = repo / 'zconf.h.in'
    if not file.resolve().is_relative_to(repo.resolve()):
        raise RuntimeError('Source path escapes dependency checkout')
    return file.read_bytes()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if (ROOT / 'public-repo').exists():
        raise RuntimeError('Use the clean public export, never the private development repository.')
    if git('status', '--porcelain').strip():
        raise RuntimeError('Commit the exact public sources before packaging a release.')
    if args.output.exists():
        raise RuntimeError('Source archive destination already exists.')
    commit = git('rev-parse', 'HEAD').decode().strip()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix='sources-', dir=ROOT / 'out'))
    # Includes platform-specific and build dependencies too, so the preserved
    # Cargo.lock remains resolvable without fetching registry sources again.
    with (stage / 'vendor.log').open('w', encoding='utf-8') as log:
        subprocess.run(['cargo', 'vendor', '--locked', '--versioned-dirs', str(stage / 'vendor')],
                       cwd=ROOT, stdout=log, stderr=log, check=True)
    info = {'vsm_commit': commit, 'steam_audio': [], 'rust_sources': 'vendor/'}
    partial = args.output.with_suffix(args.output.suffix + '.partial')
    # Registry tarballs may retain Unix-epoch mtimes, older than ZIP supports.
    # Publish the final archive name only after every source has been added.
    with zipfile.ZipFile(partial, 'x', compression=zipfile.ZIP_DEFLATED, compresslevel=6,
                         strict_timestamps=False) as archive:
        for name in git('ls-files', '-z').decode().strip('\0').split('\0'):
            archive.writestr(name, git('show', f'HEAD:{name}'))
        archive.writestr('.cargo/config.toml', '[source.crates-io]\nreplace-with = "vendored-sources"\n\n'
                           '[source.vendored-sources]\ndirectory = "vendor"\n')
        for file in (stage / 'vendor').rglob('*'):
            if file.is_file():
                archive.write(file, file.relative_to(stage).as_posix())
        for repo, revision in steam.repositories():
            names = source_files(repo)
            # Sparse Steam Audio checkout: only SDK core and root documents.
            if repo == steam.SOURCE:
                names = [n for n in names if n.startswith('core/') or '/' not in n]
            prefix = repo.relative_to(ROOT).as_posix()
            hashes = {}
            for name in names:
                data = read_source(repo, name)
                archive.writestr(prefix + '/' + name, data)
                hashes[name] = hashlib.sha256(data).hexdigest()
            archive.writestr(prefix + '/.vsm-source.json', json.dumps({'revision': revision, 'files': names}))
            info['steam_audio'].append({'path': prefix, 'revision': revision, 'sha256': hashes})
        archive.writestr('SOURCE-INFO.json', json.dumps(info, indent=2) + '\n')
        archive.writestr('SOURCE-BUILD.txt', 'VSM corresponding sources\n\n'
            'See docs/development.md for Windows build requirements.\n'
            'Rust dependencies are in vendor/ and selected by .cargo/config.toml.\n'
            'Steam Audio and its required dependency sources are in .deps/.\n'
            'Run: python tools/build_steam_audio.py\n'
            'Run: cargo build --locked --offline --release -p vsm-studio -p vsm-obs -p vsm-core\n'
            'Compiler toolchains and the OBS host are installed separately.\n'
            'The standalone build uses the official Steam Audio SDK obtained by tools/bootstrap.py;\n'
            'the OBS plugin uses the source-built .deps/steam-audio-open-4.8.1/phonon.dll.\n'
            'Dependency sources retain their original licenses. VSM source files retain MIT,\n'
            'except OBS-derived declarations as described in LICENSES.md.\n')
    partial.rename(args.output)
    print('Corresponding sources:', args.output)


if __name__ == '__main__':
    main()

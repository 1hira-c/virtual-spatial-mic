"""Build the Windows Steam Audio runtime with open-source dependencies only."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / '.deps/steam-audio-source-4.8.1'
REVISION = '0da18255cca520771f363ee01f100572b39a308e'
DEPENDENCIES = ('flatbuffers', 'pffft', 'zlib', 'mysofa')
DISABLED = ('IPP', 'MKL', 'EMBREE', 'RADEONRAYS', 'TRUEAUDIONEXT', 'FFTS')


def run(args, **kw):
    subprocess.run([str(a) for a in args], check=True, **kw)


def cmake_path():
    if found := shutil.which('cmake'):
        return Path(found)
    vswhere = Path(os.environ.get('ProgramFiles(x86)', 'C:/Program Files (x86)')) / 'Microsoft Visual Studio/Installer/vswhere.exe'
    install = subprocess.check_output([str(vswhere), '-latest', '-version', '[17.0,18.0)', '-products', '*',
        '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property', 'installationPath'], text=True).strip()
    found = Path(install) / 'Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe'
    if not found.is_file():
        raise RuntimeError('Install CMake 3.x and Visual Studio 2022 C++ build tools.')
    return found


def checkout(path, url, revision, sparse=False):
    marker = path / '.vsm-source.json'
    if marker.is_file():
        if json.loads(marker.read_text())['revision'] != revision:
            raise RuntimeError(f'Source snapshot version mismatch: {path.name}')
        return
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        run(['git', 'clone', '--filter=blob:none', '--no-checkout', url, path])
        if sparse:
            run(['git', '-C', path, 'sparse-checkout', 'set', 'core'])
        run(['git', '-C', path, 'checkout', '--detach', revision])
    actual = subprocess.check_output(['git', '-C', str(path), 'rev-parse', 'HEAD'], text=True).strip()
    if actual != revision:
        raise RuntimeError(f'Unexpected source revision: {path.name}; existing checkout left intact')


def repositories():
    deps = json.loads((SOURCE / 'core/build/dependencies.json').read_text())
    yield SOURCE, REVISION
    for name in DEPENDENCIES:
        yield SOURCE / f'core/deps-build/{name}/src/{name}', deps[name]['fetch']['tag']


def build_dependency(name):
    # The source snapshot already contains the exact dependency code. Skip the
    # upstream fetch/reset step so offline builds and user modifications work.
    script = SOURCE / 'core/build/get_dependencies.py'
    code = script.read_text()
    before = '        fetch_dependency(name, dep, platform)'
    if code.count(before) != 1:
        raise RuntimeError('Upstream dependency script changed; review the source-only build adapter')
    code = code.replace(before, '        stamp_fetch(name, platform, get_fetch_stamp(dep, platform))')
    # Old CMake minimum versions otherwise ignore MSVC_RUNTIME_LIBRARY.
    code = code.replace('    cmake_args = []', "    cmake_args = ['-DCMAKE_POLICY_DEFAULT_CMP0091=NEW']")
    # Always reconfigure: a source archive or local edits may have newer inputs
    # than the upstream script's fetch/configure stamps can represent.
    code = code.replace('    if check_dependency(name, dep, platform, debug):', '    if False:')
    os.chdir(script.parent)
    sys.argv = [str(script), '-p', 'windows', '-a', 'x64', '-t', 'vs2022', '--dependency', name]
    exec(compile(code, str(script), 'exec'), {'__name__': '__main__', '__file__': str(script)})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dependency', choices=DEPENDENCIES, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if sys.platform != 'win32':
        raise RuntimeError('This release runtime build targets Windows x64.')
    cmake = cmake_path()
    os.environ['PATH'] = str(cmake.parent) + os.pathsep + os.environ.get('PATH', '')
    if args.dependency:
        build_dependency(args.dependency)
        return
    checkout(SOURCE, 'https://github.com/ValveSoftware/steam-audio.git', REVISION, sparse=True)
    deps = json.loads((SOURCE / 'core/build/dependencies.json').read_text())
    for name in DEPENDENCIES:
        dep = deps[name]['fetch']
        path = SOURCE / f'core/deps-build/{name}/src/{name}'
        checkout(path, dep['git'], dep['tag'])
        if name == 'zlib':
            # The upstream zlib.patch omits the unbuilt shared target at install.
            file = path / 'CMakeLists.txt'
            text = file.read_text()
            original = 'install(TARGETS zlib zlibstatic'
            if original in text:
                text = text.replace(original, 'install(TARGETS zlibstatic')
            if not text.startswith('# Modified for VSM:'):
                file.write_text('# Modified for VSM: install only the static library (upstream Steam Audio zlib.patch).\n' + text)
        run([sys.executable, __file__, '--dependency', name])
    build = SOURCE / 'core/build-vsm'
    flags = [f'-DSTEAMAUDIO_ENABLE_{name}=OFF' for name in DISABLED]
    flags += [f'-DSTEAMAUDIO_BUILD_{name}=OFF' for name in ('TESTS', 'ITESTS', 'BENCHMARKS', 'SAMPLES', 'DOCS')]
    run([cmake, '-S', SOURCE / 'core', '-B', build, '-G', 'Visual Studio 17 2022', '-A', 'x64', *flags])
    run([cmake, '--build', build, '--config', 'Release', '--target', 'phonon', '--parallel', '4'])
    dll = build / 'src/core/Release/phonon.dll'
    output = ROOT / '.deps/steam-audio-open-4.8.1'
    output.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(dll, output / 'phonon.dll')
    provenance = {'version': '4.8.1', 'revision': REVISION, 'disabled': list(DISABLED),
        'fft': 'PFFFT', 'sha256': hashlib.sha256(dll.read_bytes()).hexdigest(),
        'builder_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    (output / 'provenance.json').write_text(json.dumps(provenance, indent=2) + '\n')
    print('Open-source Steam Audio runtime ready:', output)


if __name__ == '__main__':
    main()

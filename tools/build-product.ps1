<# Development build and packaging only. Product binaries need no Python, Node, Cargo, or project C++. #>
param([string]$OutputDirectory = '', [switch]$SkipBuild, [switch]$Release)
$ErrorActionPreference = 'Stop'
$project = Split-Path $PSScriptRoot -Parent
Push-Location $project
try {
    if ($Release -and $SkipBuild) { throw 'Release packaging requires a fresh product build.' }
    if ($Release -and (Test-Path -LiteralPath public-repo)) { throw 'Build releases from the public source repository.' }
    if ($Release -and (git -c ("safe.directory=" + $project) status --porcelain)) { throw 'Commit the public source tree before release packaging.' }
    $config = Get-Content -LiteralPath apps/desktop/src-tauri/tauri.conf.json -Raw | ConvertFrom-Json
    $version = $config.version
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Expected a numeric product version.' }
    if (!$SkipBuild) {
        python tools/build_steam_audio.py
        if ($LASTEXITCODE) { throw 'Open-source Steam Audio build failed.' }
        cargo build --locked --release -p vsm-studio -p vsm-obs -p vsm-core
        if ($LASTEXITCODE) { throw 'Rust product build failed.' }
    }
    $openPhonon = '.deps/steam-audio-open-4.8.1/phonon.dll'
    $provenance = Get-Content -LiteralPath .deps/steam-audio-open-4.8.1/provenance.json -Raw | ConvertFrom-Json
    if ((Get-FileHash -LiteralPath $openPhonon -Algorithm SHA256).Hash.ToLowerInvariant() -ne $provenance.sha256) { throw 'Open-source Steam Audio hash mismatch.' }
    if ($provenance.fft -ne 'PFFFT' -or @('IPP','MKL','EMBREE','RADEONRAYS','TRUEAUDIONEXT','FFTS' | Where-Object { $_ -notin $provenance.disabled }).Count) { throw 'Unexpected Steam Audio build configuration.' }
    if (!$OutputDirectory) { $OutputDirectory = Join-Path $project ('out/packages/vsm-' + $version + '-' + [DateTime]::Now.ToString('yyyyMMdd-HHmmss')) }
    $destination = [IO.Path]::GetFullPath($OutputDirectory)
    if (Test-Path -LiteralPath $destination) { throw 'Package destination must be new.' }
    $studio = Join-Path $destination 'standalone'
    $plugin = Join-Path $destination 'obs/vrc-binaural-studio'
    $pluginBin = Join-Path $plugin 'bin/64bit'
    $pluginData = Join-Path $plugin 'data'
    New-Item -ItemType Directory -Force $studio,$pluginBin,$pluginData,(Join-Path $studio 'licenses') | Out-Null
    Copy-Item -LiteralPath LICENSE,LICENSES.md -Destination $studio
    Copy-Item -LiteralPath LICENSE -Destination (Join-Path $pluginData 'LICENSE-MIT.txt')
    Copy-Item -LiteralPath LICENSES.md -Destination $pluginData
    Copy-Item -LiteralPath docs/third-party/GPL-3.0.txt -Destination (Join-Path $pluginData 'LICENSE')
    Copy-Item -LiteralPath target/release/vsm.exe -Destination $studio
    Copy-Item -LiteralPath target/release/vsm-native.exe -Destination $studio
    Copy-Item -LiteralPath target/release/vrc_binaural_studio.dll -Destination (Join-Path $pluginBin 'vrc-binaural-studio.dll')
    $phonon = '.deps/steam-audio-4.8.1/steamaudio/lib/windows-x64/phonon.dll'
    Copy-Item -LiteralPath $phonon -Destination $studio
    Copy-Item -LiteralPath $openPhonon -Destination $pluginBin
    foreach ($file in @('Steam-Audio-APACHE-2.0.txt','Steam-Audio-THIRDPARTY.md')) {
        Copy-Item -LiteralPath (Join-Path 'docs/third-party' $file) -Destination (Join-Path $studio 'licenses')
        Copy-Item -LiteralPath (Join-Path 'docs/third-party' $file) -Destination $pluginData
    }
    Copy-Item -LiteralPath docs/third-party/OBS-GPL-2.0.txt -Destination $pluginData
    Copy-Item -LiteralPath docs/third-party/GPL-3.0.txt,docs/third-party/OBS-NOTICE.txt,docs/third-party/Steam-Audio-OPEN-NOTICE.txt,docs/third-party/Steam-Audio-OPEN-DEPENDENCIES.txt -Destination $pluginData
    Copy-Item -LiteralPath .deps/steam-audio-open-4.8.1/provenance.json -Destination (Join-Path $pluginData 'Steam-Audio-build.json')
    Copy-Item -LiteralPath docs/third-party/rust-studio-NOTICES.txt,docs/third-party/rust-dependencies.json -Destination (Join-Path $studio 'licenses')
    Copy-Item -LiteralPath docs/third-party/MPL-2.0.txt -Destination (Join-Path $studio 'licenses')
    Copy-Item -LiteralPath docs/third-party/rust-obs-NOTICES.txt,docs/third-party/rust-dependencies.json -Destination $pluginData
    Copy-Item -LiteralPath docs/studio.md -Destination (Join-Path $studio 'README.md')
    Copy-Item -LiteralPath docs/obs-live.md -Destination (Join-Path $pluginData 'README.md')
    Copy-Item -LiteralPath docs/rust-runtime.md -Destination (Join-Path $destination 'runtime.md')
    $sourceArchive = $null
    if ($Release) {
        $sourceArchive = 'virtual-spatial-mic-' + $version + '-sources.zip'
        python tools/package_sources.py --output (Join-Path $destination $sourceArchive)
        if ($LASTEXITCODE) { throw 'Corresponding source packaging failed.' }
        $sourceHash = (Get-FileHash -LiteralPath (Join-Path $destination $sourceArchive) -Algorithm SHA256).Hash.ToLowerInvariant()
        $sourceText = "Corresponding sources: $sourceArchive`nSHA-256: $sourceHash`nSource commit: $(git -c ("safe.directory=" + $project) rev-parse HEAD)`n`nRedistributors: provide this source archive from the same download location as the binaries, at no additional charge. It contains VSM, Rust dependencies (including MPL-covered sources), Steam Audio and its required dependencies, and build instructions. Preserve all original licenses.`n"
    } else {
        $sourceText = "Development package: corresponding sources are not included. Before redistribution, use tools/build-product.ps1 -Release in the clean public source checkout and distribute its source archive alongside the binaries.`n"
    }
    $sourceText | Set-Content -LiteralPath (Join-Path $studio 'SOURCE.txt') -Encoding utf8
    $sourceText | Set-Content -LiteralPath (Join-Path $pluginData 'SOURCE.txt') -Encoding utf8
    # The scope document links LICENSE as the source MIT grant. In the OBS
    # binary package LICENSE is GPLv3 and the retained MIT grant has its own name.
    (Get-Content -LiteralPath (Join-Path $pluginData 'LICENSES.md') -Raw).Replace('[MIT](LICENSE)', '[MIT](LICENSE-MIT.txt)') | Set-Content -LiteralPath (Join-Path $pluginData 'LICENSES.md') -Encoding utf8
    $files = @(Get-ChildItem -LiteralPath $destination -Recurse -File | ForEach-Object {
        @{ path=[IO.Path]::GetRelativePath($destination,$_.FullName).Replace('\','/'); sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(); bytes=$_.Length }
    })
    $commit = git -c ("safe.directory=" + $project) rev-parse --verify --quiet HEAD
    if ($LASTEXITCODE) { $commit = $null }
    @{ schema_version=1; name='Virtual Spatial Mic'; version=$version; channel=$(if ($Release) { 'release-candidate' } else { 'development' }); licenses=@{standalone_own_code='MIT'; obs_combined_binary='GPL-3.0-only'}; source_archive=$sourceArchive; tested_module_version='0.2.0-preview'; runtime='Rust/Tauri'; target='windows-x64'; python_runtime=$false; node_runtime=$false; project_cpp_runtime=$false; source_commit=$commit; source_dirty=[bool](git -c ("safe.directory=" + $project) status --porcelain); files=$files } |
        ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $destination 'manifest.json') -Encoding utf8
    Write-Output "Product package: $destination"
} finally { Pop-Location }

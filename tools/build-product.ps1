<# Development build and packaging only. Product binaries need no Python, Node, Cargo, or project C++. #>
param([string]$OutputDirectory = '', [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$project = Split-Path $PSScriptRoot -Parent
Push-Location $project
try {
    $config = Get-Content -LiteralPath apps/desktop/src-tauri/tauri.conf.json -Raw | ConvertFrom-Json
    $version = $config.version
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Expected a numeric product version.' }
    if (!$SkipBuild) {
        cargo build --locked --release -p vsm-studio -p vsm-obs -p vsm-core
        if ($LASTEXITCODE) { throw 'Rust product build failed.' }
    }
    if (!$OutputDirectory) { $OutputDirectory = Join-Path $project ('out/packages/vsm-' + $version + '-' + [DateTime]::Now.ToString('yyyyMMdd-HHmmss')) }
    $destination = [IO.Path]::GetFullPath($OutputDirectory)
    if (Test-Path -LiteralPath $destination) { throw 'Package destination must be new.' }
    $studio = Join-Path $destination 'standalone'
    $plugin = Join-Path $destination 'obs/vrc-binaural-studio'
    $pluginBin = Join-Path $plugin 'bin/64bit'
    $pluginData = Join-Path $plugin 'data'
    New-Item -ItemType Directory -Force $studio,$pluginBin,$pluginData,(Join-Path $studio 'licenses') | Out-Null
    Copy-Item -LiteralPath LICENSE,LICENSES.md -Destination $studio
    Copy-Item -LiteralPath LICENSE,LICENSES.md -Destination $pluginData
    Copy-Item -LiteralPath target/release/vsm.exe -Destination $studio
    Copy-Item -LiteralPath target/release/vsm-native.exe -Destination $studio
    Copy-Item -LiteralPath target/release/vrc_binaural_studio.dll -Destination (Join-Path $pluginBin 'vrc-binaural-studio.dll')
    $phonon = '.deps/steam-audio-4.8.1/steamaudio/lib/windows-x64/phonon.dll'
    Copy-Item -LiteralPath $phonon -Destination $studio
    Copy-Item -LiteralPath $phonon -Destination $pluginBin
    foreach ($file in @('Steam-Audio-APACHE-2.0.txt','Steam-Audio-THIRDPARTY.md')) {
        Copy-Item -LiteralPath (Join-Path 'docs/third-party' $file) -Destination (Join-Path $studio 'licenses')
        Copy-Item -LiteralPath (Join-Path 'docs/third-party' $file) -Destination $pluginData
    }
    Copy-Item -LiteralPath docs/third-party/OBS-GPL-2.0.txt -Destination $pluginData
    Copy-Item -LiteralPath docs/third-party/rust-studio-NOTICES.txt,docs/third-party/rust-dependencies.json -Destination (Join-Path $studio 'licenses')
    Copy-Item -LiteralPath docs/third-party/rust-obs-NOTICES.txt,docs/third-party/rust-dependencies.json -Destination $pluginData
    Copy-Item -LiteralPath docs/studio.md -Destination (Join-Path $studio 'README.md')
    Copy-Item -LiteralPath docs/obs-live.md -Destination (Join-Path $pluginData 'README.md')
    Copy-Item -LiteralPath docs/rust-runtime.md -Destination (Join-Path $destination 'runtime.md')
    $files = @(Get-ChildItem -LiteralPath $destination -Recurse -File | ForEach-Object {
        @{ path=[IO.Path]::GetRelativePath($destination,$_.FullName).Replace('\','/'); sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(); bytes=$_.Length }
    })
    $commit = git rev-parse --verify --quiet HEAD
    if ($LASTEXITCODE) { $commit = $null }
    @{ schema_version=1; name='Virtual Spatial Mic'; version=$version; channel='development'; tested_module_version='0.2.0-preview'; runtime='Rust/Tauri'; target='windows-x64'; python_runtime=$false; node_runtime=$false; project_cpp_runtime=$false; source_commit=$commit; source_dirty=[bool](git status --porcelain); files=$files } |
        ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $destination 'manifest.json') -Encoding utf8
    Write-Output "Product package: $destination"
} finally { Pop-Location }

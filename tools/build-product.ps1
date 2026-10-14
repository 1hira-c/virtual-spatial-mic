<# Development build and packaging only. Product binaries need no Python, Node, or C++ runtime. #>
param([string]$OutputDirectory = '', [switch]$SkipBuild, [switch]$Release)
$ErrorActionPreference = 'Stop'
$project = Split-Path $PSScriptRoot -Parent
$hadEncodedRustFlags = Test-Path -LiteralPath Env:CARGO_ENCODED_RUSTFLAGS
$originalEncodedRustFlags = $env:CARGO_ENCODED_RUSTFLAGS
Push-Location $project
try {
    if ($Release -and $SkipBuild) { throw 'Release packaging requires a fresh product build.' }
    if ($Release -and (Test-Path -LiteralPath public-repo)) { throw 'Build releases from the public source repository.' }
    if ($Release -and (git -c ("safe.directory=" + $project) status --porcelain)) { throw 'Commit the public source tree before release packaging.' }
    $config = Get-Content -LiteralPath apps/desktop/src-tauri/tauri.conf.json -Raw | ConvertFrom-Json
    $version = $config.version
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Expected a numeric product version.' }
    if ($Release) {
        # Preserve Cargo's flag precedence and spaces in remapped Windows paths.
        [string[]]$flags = if ($hadEncodedRustFlags) { if ($originalEncodedRustFlags) { $originalEncodedRustFlags -split [char]31 } else { @() } }
            elseif ($env:RUSTFLAGS) { @($env:RUSTFLAGS -split '\s+' | Where-Object { $_ }) } else { @() }
        # rustc uses the last matching prefix: put the narrower project mapping last.
        foreach ($mapping in @(@{from=$env:USERPROFILE; to='/build/user'}, @{from=$project; to='/virtual-spatial-mic'})) {
            if (!$mapping.from) { continue }
            foreach ($prefix in @($mapping.from.Replace('\','/'), $mapping.from.Replace('/','\'))) {
                $flags += '--remap-path-prefix=' + $prefix + '=' + $mapping.to
            }
        }
        $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]31
    }
    if (!$SkipBuild) {
        cargo build --locked --release -p vsm-studio -p vsm-core
        if ($LASTEXITCODE) { throw 'Rust product build failed.' }
    }
    if ($Release) {
        # Committed notices must describe exactly the crates in this build.
        $check = Join-Path $project ('out/notices-' + [DateTime]::Now.ToString('yyyyMMdd-HHmmss'))
        python tools/rust_notices.py $check
        if ($LASTEXITCODE) { throw 'Rust notice generation failed.' }
        foreach ($file in @('rust-NOTICES.txt', 'rust-dependencies.json')) {
            if ((Get-FileHash -LiteralPath (Join-Path $check $file)).Hash -ne (Get-FileHash -LiteralPath (Join-Path 'docs/third-party' $file)).Hash) { throw "docs/third-party/$file is stale; run python tools/rust_notices.py docs/third-party" }
        }
    }
    if (!$OutputDirectory) { $OutputDirectory = Join-Path $project ('out/packages/vsm-' + $version + '-' + [DateTime]::Now.ToString('yyyyMMdd-HHmmss')) }
    $destination = [IO.Path]::GetFullPath($OutputDirectory)
    if (Test-Path -LiteralPath $destination) { throw 'Package destination must be new.' }
    $app = Join-Path $destination 'Virtual Spatial Mic'
    $licenses = Join-Path $app 'licenses'
    New-Item -ItemType Directory -Force $app,$licenses | Out-Null
    Copy-Item -LiteralPath target/release/vsm.exe,target/release/vsm-native.exe,LICENSE -Destination $app
    # The scope document links into the repository; the package keeps notices in licenses/.
    (Get-Content -LiteralPath LICENSES.md -Raw).Replace('](docs/third-party/', '](licenses/') | Set-Content -LiteralPath (Join-Path $app 'LICENSES.md') -Encoding utf8
    Copy-Item -LiteralPath docs/studio.md -Destination (Join-Path $app 'README.md')
    Copy-Item -LiteralPath docs/obs-live.md -Destination (Join-Path $app 'OBS.md')
    foreach ($file in @('rust-NOTICES.txt','rust-dependencies.json','MPL-2.0.txt','Apache-2.0.txt','SADIE-II-NOTICE.txt','libmysofa-BSD-3-Clause.txt','Spout-BSD-2-Clause.txt','Spout-NOTICE.txt')) {
        Copy-Item -LiteralPath (Join-Path 'docs/third-party' $file) -Destination $licenses
    }
    $commit = git -c ("safe.directory=" + $project) rev-parse --verify --quiet HEAD
    if ($LASTEXITCODE) { $commit = $null }
    $sourceArchive = $null
    if ($Release) {
        $sourceArchive = 'virtual-spatial-mic-' + $version + '-mpl-sources.zip'
        python tools/package_sources.py --output (Join-Path $destination $sourceArchive)
        if ($LASTEXITCODE) { throw 'MPL source packaging failed.' }
        $sourceHash = (Get-FileHash -LiteralPath (Join-Path $destination $sourceArchive) -Algorithm SHA256).Hash.ToLowerInvariant()
        $sourceText = "Virtual Spatial Mic $version`nSource commit: $commit`n`nMPL-2.0 covered crate sources: $sourceArchive`nSHA-256: $sourceHash`n`nRedistributors: provide this archive from the same download location as the binaries, at no additional charge. The crates are used unmodified; see licenses/rust-dependencies.json (mpl_crates). VSM's own source is MIT; build it with cargo as described in docs/development.md.`n"
    } else {
        $sourceText = "Development package: the MPL-2.0 source archive is not included. Before redistribution, run tools/build-product.ps1 -Release in the clean public source checkout and distribute its source archive alongside the binaries.`n"
    }
    $sourceText | Set-Content -LiteralPath (Join-Path $app 'SOURCE.txt') -Encoding utf8
    $files = @(Get-ChildItem -LiteralPath $destination -Recurse -File | ForEach-Object {
        @{ path=[IO.Path]::GetRelativePath($destination,$_.FullName).Replace('\','/'); sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(); bytes=$_.Length }
    })
    @{ schema_version=2; name='Virtual Spatial Mic'; version=$version; channel=$(if ($Release) { 'release-candidate' } else { 'development' }); licenses=@{own_code='MIT'; mpl_crates_source=$sourceArchive; hrtf='SADIE II D1 (Apache-2.0)'}; spatializer='sofar 0.3.0 (MIT OR Apache-2.0)'; tested_module_version='0.2.0-preview'; runtime='Rust/Tauri'; target='windows-x64'; python_runtime=$false; node_runtime=$false; cpp_runtime=$false; source_commit=$commit; source_dirty=[bool](git -c ("safe.directory=" + $project) status --porcelain); files=$files } |
        ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $destination 'manifest.json') -Encoding utf8
    Write-Output "Product package: $destination"
} finally {
    if ($Release) {
        if ($hadEncodedRustFlags) { $env:CARGO_ENCODED_RUSTFLAGS = $originalEncodedRustFlags }
        else { Remove-Item -LiteralPath Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue }
    }
    Pop-Location
}

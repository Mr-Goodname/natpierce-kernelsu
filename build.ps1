param(
    [switch]$SkipBuild,
    [switch]$Tests,
    [string]$CargoPath,
    [string]$RustcPath,
    [string]$LinkerPath,
    [string]$Toolchain = '1.99.1'
)

$ErrorActionPreference = 'Stop'
$projectRoot = $PSScriptRoot
$moduleRoot = Join-Path $projectRoot 'module'
$outputRoot = Join-Path $projectRoot 'dist'
$managerBinary = Join-Path $moduleRoot 'bin/npctl'
$toolchainVersion = '1.99.1'

function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Assert-Arm64Static([string]$Path) {
    $data = [IO.File]::ReadAllBytes($Path)
    if ($data.Length -lt 64 -or $data[0] -ne 0x7f -or $data[1] -ne 0x45 -or $data[2] -ne 0x4c -or $data[3] -ne 0x46 -or $data[4] -ne 2 -or $data[5] -ne 1 -or [BitConverter]::ToUInt16($data,18) -ne 183) {
        throw "Expected a little-endian ARM64 ELF executable: $Path"
    }
    $headerOffset = [BitConverter]::ToUInt64($data,32)
    $headerSize = [BitConverter]::ToUInt16($data,54)
    $headerCount = [BitConverter]::ToUInt16($data,56)
    if ($headerSize -lt 56 -or $headerOffset -gt $data.LongLength -or $headerCount * [uint64]$headerSize -gt $data.LongLength - $headerOffset) { throw "Invalid ELF program headers: $Path" }
    for ($index=0; $index -lt $headerCount; $index++) {
        if ([BitConverter]::ToUInt32($data,[int]($headerOffset+$index*$headerSize)) -eq 3) { throw "Dynamic ELF loader is unsupported: $Path" }
    }
}

function Compile-Manager {
    $bundledRust = Join-Path $projectRoot 'tools/rust'
    if (-not $CargoPath) {
        $localCargo = Join-Path $bundledRust 'bin/cargo.exe'
        if (Test-Path -LiteralPath $localCargo) { $CargoPath = $localCargo } else { $CargoPath = (Get-Command cargo -ErrorAction Stop).Source }
    }
    if (-not $RustcPath) {
        $localRustc = Join-Path $bundledRust 'bin/rustc.exe'
        if (Test-Path -LiteralPath $localRustc) { $RustcPath = $localRustc } else { $RustcPath = (Get-Command rustc -ErrorAction Stop).Source }
    }
    $savedVariables = @{}
    $envNames = @('RUSTC','RUSTUP_TOOLCHAIN','CARGO_HOME','CARGO_TARGET_DIR','CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER','CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS')
    foreach ($name in $envNames) { $savedVariables[$name] = [Environment]::GetEnvironmentVariable($name,'Process') }
    try {
        $env:RUSTUP_TOOLCHAIN = $Toolchain
        $env:RUSTC = $RustcPath
        $compilerVersion = & $RustcPath --version
        if ($LASTEXITCODE -ne 0 -or $compilerVersion -notmatch '^rustc 1\.99\.1\b') { throw "Install Rust $toolchainVersion and its aarch64-unknown-linux-musl target; received: $compilerVersion" }
        if (-not $LinkerPath) {
            $sysroot = (& $RustcPath --print sysroot).Trim()
            $compilerDetails = & $RustcPath -vV
            $rustHostTriple = (($compilerDetails | Where-Object { $_ -match '^host: ' }) -replace '^host: ','').Trim()
            $linkerName = if ($rustHostTriple -match 'windows') { 'rust-lld.exe' } else { 'rust-lld' }
            $LinkerPath = Join-Path $sysroot "lib/rustlib/$rustHostTriple/bin/$linkerName"
        }
        if (-not (Test-Path -LiteralPath $LinkerPath)) { throw "Rust LLVM linker missing: $LinkerPath" }
        if (Test-Path -LiteralPath (Join-Path $projectRoot 'tools')) { $env:CARGO_HOME = Join-Path $projectRoot 'tools/cargo-cache' }
        $env:CARGO_TARGET_DIR = Join-Path $projectRoot 'target'
        $env:CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER = $LinkerPath
        $env:CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = '-C target-feature=+crt-static'
        $manifest = Join-Path $projectRoot 'Cargo.toml'
        if (-not (Test-Path -LiteralPath (Join-Path $projectRoot 'Cargo.lock'))) { throw 'Cargo.lock is required for reproducible dependency resolution' }
        if ($Tests) {
            & $CargoPath test --manifest-path $manifest --target aarch64-unknown-linux-musl --no-run --locked
            if ($LASTEXITCODE -ne 0) { throw 'Rust ARM64 test compilation failed' }
        }
        & $CargoPath build --manifest-path $manifest --target aarch64-unknown-linux-musl --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'Rust release compilation failed' }
        New-Item -ItemType Directory -Force (Split-Path -Parent $managerBinary) | Out-Null
        Copy-Item -LiteralPath (Join-Path $projectRoot 'target/aarch64-unknown-linux-musl/release/npctl') -Destination $managerBinary -Force
    } finally {
        foreach ($name in $envNames) { [Environment]::SetEnvironmentVariable($name,$savedVariables[$name],'Process') }
    }
}

function Add-ZipFile($Zip,[string]$Name,[string]$Path,[bool]$Executable) {
    $entry = $Zip.CreateEntry($Name,[IO.Compression.CompressionLevel]::Optimal)
    # Stable metadata permits identical packaging for unchanged inputs.
    $entry.LastWriteTime = [DateTimeOffset]::new(2000,1,1,0,0,0,[TimeSpan]::Zero)
    $mode = if ($Executable) { [int64]0x81ed } else { [int64]0x81a4 }
    $attributes = $mode -shl 16
    if ($attributes -gt [int]::MaxValue) { $attributes -= [int64]0x100000000 }
    $entry.ExternalAttributes = [int]$attributes
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($Name -match '\.(sh|prop|html|css|js|svg|md|txt|toml|lock|rs|ps1)$' -or $Name.EndsWith('/.gitignore')) {
        $text = [Text.Encoding]::UTF8.GetString($bytes).TrimStart([char]0xfeff).Replace("`r`n","`n")
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($text)
    }
    $stream = $entry.Open()
    try { $stream.Write($bytes,0,$bytes.Length) } finally { $stream.Dispose() }
}

function Write-Zip([string]$Path,$Entries) {
    $file = [IO.File]::Open($Path,[IO.FileMode]::Create,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
    try {
        $zip = [IO.Compression.ZipArchive]::new($file,[IO.Compression.ZipArchiveMode]::Create,$false,[Text.UTF8Encoding]::new($false))
        try {
            foreach ($item in ($Entries | Sort-Object Name -CaseSensitive)) { Add-ZipFile $zip $item.Name $item.Path $item.Executable }
        } finally { $zip.Dispose() }
    } finally { $file.Dispose() }
}

if (-not $SkipBuild) { Compile-Manager }
if (-not (Test-Path -LiteralPath $managerBinary)) { throw 'Missing module/bin/npctl; build first' }
Assert-Arm64Static $managerBinary

$metadata = [IO.File]::ReadAllText((Join-Path $moduleRoot 'module.prop'))
$moduleId = [regex]::Match($metadata,'(?m)^id=([a-zA-Z0-9_]+)\r?$').Groups[1].Value
$moduleVersion = [regex]::Match($metadata,'(?m)^version=([a-zA-Z0-9._-]+)\r?$').Groups[1].Value
if (-not $moduleId -or -not $moduleVersion) { throw 'Invalid module metadata' }

$moduleEntries = @()
$sourceEntries = @()
$allowedModuleFiles = @('module.prop','customize.sh','service.sh','uninstall.sh','bin/watchdog.sh','webroot/index.html','webroot/app.js','webroot/style.css','bin/npctl','licenses/LICENSE','licenses/THIRD_PARTY_LICENSES.md')
foreach ($file in (Get-ChildItem -LiteralPath $moduleRoot -File -Recurse)) {
    $relative = $file.FullName.Substring($moduleRoot.Length+1).Replace('\','/')
    if ($relative -cnotin $allowedModuleFiles) { throw "Unexpected module path refused to prevent private data inclusion: $relative" }
    $binary = $relative -eq 'bin/npctl'
    $item = @{ Name=$relative; Path=$file.FullName; Executable=($binary -or $relative.EndsWith('.sh')) }
    $moduleEntries += $item
    if (-not $binary) { $sourceEntries += @{ Name="natpierce-kernelsu/module/$relative"; Path=$file.FullName; Executable=$item.Executable } }
}
foreach ($required in $allowedModuleFiles) {
    if ($required -notin $moduleEntries.Name) { throw "Required module file missing: $required" }
}
foreach ($rootFile in @('Cargo.toml','Cargo.lock','build.ps1','README.md','NOTICE.md','VALIDATION.md','LICENSE','THIRD_PARTY_LICENSES.md','core-manifest.json','.gitignore')) {
    $path = Join-Path $projectRoot $rootFile
    if (-not (Test-Path -LiteralPath $path)) { throw "Required source file missing: $rootFile" }
    $sourceEntries += @{ Name="natpierce-kernelsu/$rootFile"; Path=$path; Executable=$false }
}
$publicExtras = @('docs/screenshots/webui.png','.github/workflows/build.yml')
foreach ($relative in $publicExtras) {
    $path = Join-Path $projectRoot $relative
    if (-not (Test-Path -LiteralPath $path)) { throw "Required public file missing: $relative" }
    $sourceEntries += @{ Name="natpierce-kernelsu/$relative"; Path=$path; Executable=$false }
}
$sourceRoot = Join-Path $projectRoot 'src'
foreach ($file in (Get-ChildItem -LiteralPath $sourceRoot -File -Recurse -Filter '*.rs')) {
    $relative = $file.FullName.Substring($sourceRoot.Length+1).Replace('\','/')
    $sourceEntries += @{ Name="natpierce-kernelsu/src/$relative"; Path=$file.FullName; Executable=$false }
}

New-Item -ItemType Directory -Force $outputRoot | Out-Null
$moduleZip = Join-Path $outputRoot "${moduleId}_${moduleVersion}.zip"
$sourceZip = Join-Path $outputRoot "${moduleId}_${moduleVersion}_source.zip"
Write-Zip $moduleZip $moduleEntries
Write-Zip $sourceZip $sourceEntries
$checksumPath = Join-Path $outputRoot "${moduleId}_${moduleVersion}_SHA256SUMS.txt"
$checksumLines = @()
$checksumLines += (Get-Sha256 $moduleZip)+'  '+[IO.Path]::GetFileName($moduleZip)
$checksumLines += (Get-Sha256 $sourceZip)+'  '+[IO.Path]::GetFileName($sourceZip)
[IO.File]::WriteAllText($checksumPath,($checksumLines -join "`n")+"`n",[Text.UTF8Encoding]::new($false))
Write-Output "Module: $moduleZip"
Write-Output "Source: $sourceZip"
Write-Output "Checksums: $checksumPath"
$checksumLines | Write-Output

# Downloads the pinned source snapshots that Cargo.toml points at into third_party\.
# Run once after cloning: powershell -ExecutionPolicy Bypass -File scripts\fetch_deps.ps1
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'third_party'

$deps = @(
    @{ Name = 'zed';      Repo = 'zed-industries/zed';      Rev = 'f8cfc7ed9e57503def9479138894f3139f5400cc' },
    @{ Name = 'wgpu';     Repo = 'gfx-rs/wgpu';             Rev = 'cddb2f5d80dc0009b2ef6767635140cacf90a007' },
    @{ Name = 'font-kit'; Repo = 'zed-industries/font-kit'; Rev = '94b0f28166665e8fd2f53ff6d268a14955c82269' }
)

New-Item -ItemType Directory -Force $dest | Out-Null
Add-Type -AssemblyName System.IO.Compression.FileSystem
foreach ($d in $deps) {
    $target = Join-Path $dest $d.Name
    if (Test-Path $target) {
        Write-Host "$($d.Name): already present, skipped"
        continue
    }
    $url = "https://github.com/$($d.Repo)/archive/$($d.Rev).zip"
    $zip = Join-Path $dest "$($d.Name).zip"
    $tmp = Join-Path $dest "$($d.Name).tmp"
    Write-Host "$($d.Name): downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    [IO.Compression.ZipFile]::ExtractToDirectory($zip, $tmp)
    # The archive holds a single <repo>-<rev> folder.
    $inner = Get-ChildItem $tmp -Directory | Select-Object -First 1
    Move-Item $inner.FullName $target
    Remove-Item -Recurse -Force $tmp, $zip
    Write-Host "$($d.Name): done"
}

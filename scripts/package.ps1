# Builds a release and packs it as dist\SwiftFind-<version>-x64.zip.
# Run from anywhere: powershell -ExecutionPolicy Bypass -File scripts\package.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$name = "SwiftFind-$version-x64"
$stage = Join-Path $root "dist\$name"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item target\release\swiftfind.exe (Join-Path $stage 'SwiftFind.exe')
# README in UTF-8 with BOM so Notepad shows the Chinese text correctly.
$readme = Get-Content -Raw -Encoding UTF8 scripts\README.txt
[IO.File]::WriteAllText((Join-Path $stage '使用说明.txt'), $readme.Replace('{VERSION}', $version), (New-Object Text.UTF8Encoding $true))

$zip = Join-Path $root "dist\$name.zip"
Remove-Item -Force $zip -ErrorAction SilentlyContinue
# Entries added one by one: Compress-Archive and ZipFile.CreateFromDirectory
# in Windows PowerShell 5 write '\' as the path separator.
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::Open($zip, 'Create', [Text.Encoding]::UTF8)
try {
    foreach ($file in Get-ChildItem $stage -File) {
        [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $file.FullName, "$name/$($file.Name)", 'Optimal') | Out-Null
    }
} finally {
    $archive.Dispose()
}
Write-Host "Created $zip ($([math]::Round((Get-Item $zip).Length / 1MB, 1)) MB)"

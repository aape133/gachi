# Builds the gachi VST3 plugin, lays it out as a VST3 bundle and zips it for the releases
# page (the plugin is a download of its own; the app's installer does not carry it):
#   target\release\vst3\gachi.vst3\Contents\x86_64-win\gachi.vst3
#   target\release\vst3\gachi_<version>_vst3.zip   (the gachi.vst3 folder, to unpack into
#                                                    the DAW's VST3 folder)
#
#   powershell -ExecutionPolicy Bypass -File crates\plugin\bundle.ps1
#
# To try a build without installing: add target\release\vst3 as a plug-in folder in the DAW.

$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Push-Location $root
try {
    cargo build -p gachi-plugin --release
    if ($LASTEXITCODE -ne 0) { throw "build failed" }
    $out = Join-Path $root 'target\release\vst3'
    $bundle = Join-Path $out 'gachi.vst3'
    $bin = Join-Path $bundle 'Contents\x86_64-win'
    New-Item -ItemType Directory -Force $bin | Out-Null
    Copy-Item (Join-Path $root 'target\release\gachi_plugin.dll') (Join-Path $bin 'gachi.vst3') -Force
    # the license travels inside the bundle, so that nothing else lands in the VST3 folder
    $res = Join-Path $bundle 'Contents\Resources'
    New-Item -ItemType Directory -Force $res | Out-Null
    Copy-Item (Join-Path $root 'LICENSE') (Join-Path $res 'LICENSE.txt') -Force
    # (and the licenses of what it is built with, their texts included)
    Copy-Item (Join-Path $root 'THIRD-PARTY-LICENSES.md') (Join-Path $res 'THIRD-PARTY-LICENSES.md') -Force
    Write-Host "bundle: $bundle"

    $version = (Get-Content (Join-Path $root 'app\tauri.conf.json') -Raw -Encoding UTF8 | ConvertFrom-Json).version
    Get-ChildItem $out -Filter 'gachi_*_vst3.zip' | Remove-Item -Force
    $zip = Join-Path $out "gachi_${version}_vst3.zip"
    Compress-Archive -Path $bundle -DestinationPath $zip
    Write-Host "zip: $zip"
} finally {
    Pop-Location
}

$ErrorActionPreference = 'Stop'
if (-not $env:WROSECODE_RELEASE_BASE) { throw 'Set WROSECODE_RELEASE_BASE to the published release-asset URL' }
$base = $env:WROSECODE_RELEASE_BASE
$arch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()) { 'X64' { 'x86_64' } 'Arm64' { 'aarch64' } default { throw 'Unsupported architecture' } }
$dest = if ($env:WROSECODE_INSTALL_DIR) { $env:WROSECODE_INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Invoke-WebRequest -Uri "$base/wrosecode-windows-$arch.exe" -OutFile (Join-Path $dest 'wrosecode.exe')
Write-Host "Installed $dest\wrosecode.exe"

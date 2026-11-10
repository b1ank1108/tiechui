# Installs chui.exe and tiechui.exe, plus the bundled rg.exe, from a GitHub release.
# Usage: irm https://github.com/b1ank1108/tiechui/releases/latest/download/install.ps1 | iex
# Environment: TIECHUI_VERSION (default: latest), TIECHUI_INSTALL_DIR (default: %LOCALAPPDATA%\Programs\tiechui).
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'https://github.com/b1ank1108/tiechui'
$dir = if ($env:TIECHUI_INSTALL_DIR) { $env:TIECHUI_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\tiechui' }

# x64 binaries also run under emulation on Windows on Arm.
$cpu = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($cpu -notin 'AMD64', 'ARM64') { throw "Unsupported CPU: $cpu" }

$version = $env:TIECHUI_VERSION
if (-not $version) {
    $response = Invoke-WebRequest -Uri "$repo/releases/latest" -Method Head -UseBasicParsing
    $final = if ($response.BaseResponse.ResponseUri) { $response.BaseResponse.ResponseUri.AbsoluteUri } else { $response.BaseResponse.RequestMessage.RequestUri.AbsoluteUri }
    $version = $final.Split('/')[-1]
}
$version = $version.TrimStart('v')
if ($version -notmatch '^\d') { throw 'Could not determine the latest version; set TIECHUI_VERSION' }

$asset = "tiechui-$version-x86_64-pc-windows-msvc.zip"
$temp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    Write-Host "Downloading $asset"
    Invoke-WebRequest -Uri "$repo/releases/download/v$version/$asset" -OutFile (Join-Path $temp $asset) -UseBasicParsing
    Invoke-WebRequest -Uri "$repo/releases/download/v$version/SHA256SUMS" -OutFile (Join-Path $temp 'SHA256SUMS') -UseBasicParsing

    $expected = Get-Content (Join-Path $temp 'SHA256SUMS') | ForEach-Object {
        $fields = $_ -split '\s+'
        if ($fields[1] -eq $asset) { $fields[0] }
    }
    $actual = (Get-FileHash (Join-Path $temp $asset) -Algorithm SHA256).Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) { throw "Checksum mismatch for $asset" }

    Expand-Archive -Path (Join-Path $temp $asset) -DestinationPath (Join-Path $temp 'out')
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    foreach ($name in 'chui.exe', 'tiechui.exe') {
        Copy-Item -Path (Join-Path $temp "out\$name") -Destination (Join-Path $dir $name) -Force
    }
    # chui finds the bundled rg.exe beside itself; older releases do not bundle it.
    $rg = Join-Path $temp 'out\rg.exe'
    if (Test-Path -LiteralPath $rg) {
        Copy-Item -Path $rg -Destination (Join-Path $dir 'rg.exe') -Force
    }
} finally {
    Remove-Item -Recurse -Force $temp
}
Write-Host "Installed chui $version to $dir"

$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($path -split ';') -contains $dir)) {
    [Environment]::SetEnvironmentVariable('Path', (@($path, $dir) | Where-Object { $_ }) -join ';', 'User')
    Write-Host "Added $dir to the user Path; open a new terminal to use chui."
}
if (-not (Get-Command bash.exe -ErrorAction SilentlyContinue) -and -not $env:TIECHUI_GIT_BASH_PATH) {
    Write-Host 'The bash tool needs Git for Windows: https://git-scm.com/download/win'
}

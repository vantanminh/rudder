# Install the native `rudder` CLI on Windows.
#
#   irm https://github.com/vantanminh/rudder/releases/latest/download/install-cli.ps1 | iex
#
# Environment variables:
#   $env:RUDDER_GITHUB_REPO   GitHub repo (default: vantanminh/rudder)
#   $env:RUDDER_CLI_VERSION   Release tag (default: latest)
#   $env:RUDDER_CLI_BIN_DIR   Install directory (default: $HOME\.rudder\bin)

$ErrorActionPreference = "Stop"

$Repo = if ($env:RUDDER_GITHUB_REPO) { $env:RUDDER_GITHUB_REPO } else { "vantanminh/rudder" }
$Version = if ($env:RUDDER_CLI_VERSION) { $env:RUDDER_CLI_VERSION } else { "latest" }
$BinDir = if ($env:RUDDER_CLI_BIN_DIR) { $env:RUDDER_CLI_BIN_DIR } else { Join-Path $HOME ".rudder\bin" }

$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    "ARM64" { "aarch64" }
    default { throw "Unsupported Windows architecture: $($env:PROCESSOR_ARCHITECTURE)" }
}

$Asset = "rudder-$Arch-pc-windows-msvc.exe"
$BaseUrl = if ($Version -eq "latest") {
    "https://github.com/$Repo/releases/latest/download"
} else {
    $Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
    "https://github.com/$Repo/releases/download/$Tag"
}

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("rudder-cli-" + [guid]::NewGuid().ToString())
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $BinTemp = Join-Path $TmpDir $Asset
    $SumsTemp = Join-Path $TmpDir "SHA256SUMS"

    Write-Host "rudder-install: downloading $Asset from $BaseUrl..."
    Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $BinTemp -UseBasicParsing
    Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile $SumsTemp -UseBasicParsing

    $Expected = $null
    foreach ($Line in (Get-Content $SumsTemp)) {
        $Parts = $Line -split "\s+", 2
        if ($Parts.Length -eq 2 -and ($Parts[1].TrimStart('*') -eq $Asset)) {
            $Expected = $Parts[0].ToLowerInvariant()
            break
        }
    }
    if (-not $Expected) {
        throw "SHA256SUMS does not list a checksum for $Asset"
    }

    $Actual = (Get-FileHash -Path $BinTemp -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        throw "SHA-256 mismatch for $Asset (expected $Expected, got $Actual)"
    }

    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $Dest = Join-Path $BinDir "rudder.exe"
    Move-Item -Force -Path $BinTemp -Destination $Dest
    Write-Host "rudder-install: installed rudder to $Dest"

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not ($UserPath -split ";" | Where-Object { $_ -eq $BinDir })) {
        $NewPath = if ($UserPath) { "$UserPath;$BinDir" } else { $BinDir }
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$env:Path;$BinDir"
        Write-Host "rudder-install: added $BinDir to your user PATH"
    }

    & $Dest --version
} finally {
    Remove-Item -Recurse -Force -Path $TmpDir -ErrorAction SilentlyContinue
}

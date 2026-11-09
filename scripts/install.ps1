# Install agent-browser-use on Windows into %LOCALAPPDATA%\agent-browser-use. No admin needed.
#   irm https://raw.githubusercontent.com/cavaldos/agent-browser-use/main/scripts/install.ps1 | iex
#   & ([scriptblock]::Create((irm .../install.ps1))) v1.2.3     # a specific version
param([string]$Version = "latest")
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "cavaldos/agent-browser-use"
$BinDir = Join-Path $env:LOCALAPPDATA "agent-browser-use"

$Arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "amd64" }

if ($Version -eq "latest") {
    $Version = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name
}
$Name = "agent-browser-use_${Version}_windows_${Arch}"
$Base = "https://github.com/$Repo/releases/download/$Version"

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    Write-Host "Downloading $Name ..."
    Invoke-WebRequest "$Base/$Name.zip" -OutFile "$Tmp\$Name.zip"
    Invoke-WebRequest "$Base/SHA256SUMS" -OutFile "$Tmp\SHA256SUMS"

    $line = Get-Content "$Tmp\SHA256SUMS" | Where-Object { $_ -match " $([regex]::Escape($Name)).zip$" }
    if (-not $line) { throw "$Name.zip is not listed in SHA256SUMS" }
    $expected = ($line -split "\s+")[0]
    $actual = (Get-FileHash "$Tmp\$Name.zip" -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) { throw "checksum mismatch, download discarded" }

    Expand-Archive "$Tmp\$Name.zip" -DestinationPath $Tmp -Force
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Copy-Item "$Tmp\$Name\agent-browser-use.exe" "$BinDir\agent-browser-use.exe" -Force
} finally {
    Remove-Item $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $BinDir) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$BinDir", "User")
    Write-Host "Added $BinDir to your PATH (open a new terminal)."
}
Write-Host "Installed: $BinDir\agent-browser-use.exe ($Version)"

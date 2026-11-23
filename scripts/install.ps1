# Install agent-browser-use on Windows into %LOCALAPPDATA%\agent-browser-use. No admin needed.
#   irm https://raw.githubusercontent.com/cavaldos/agent-browser-use/main/scripts/install.ps1 | iex
#   & ([scriptblock]::Create((irm .../install.ps1))) v1.2.3     # a specific version
# Downloads the raw release binary agent-browser-use-windows-amd64.exe (only amd64 is published;
# on Windows ARM it runs through x64 emulation).
param([string]$Version = "latest")
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "cavaldos/agent-browser-use"
$BinDir = Join-Path $env:LOCALAPPDATA "agent-browser-use"
$Name = "agent-browser-use-windows-amd64.exe"

if ($Version -eq "latest") {
    $Url = "https://github.com/$Repo/releases/latest/download/$Name"
} else {
    $Url = "https://github.com/$Repo/releases/download/$Version/$Name"
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$Dest = Join-Path $BinDir "agent-browser-use.exe"
$Part = "$Dest.download"
try {
    Write-Host "Downloading $Url ..."
    Invoke-WebRequest $Url -OutFile $Part
    Move-Item $Part $Dest -Force
} finally {
    Remove-Item $Part -Force -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $BinDir) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$BinDir", "User")
    Write-Host "Added $BinDir to your PATH (open a new terminal)."
}
Write-Host "Installed: $Dest ($Version)"

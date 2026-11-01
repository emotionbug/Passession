$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Split-Path -Parent $ScriptDir
Push-Location $RepoRoot
try {
    & cargo xtask dev daemon --watch @args
    exit $LASTEXITCODE
} finally {
    Pop-Location
}

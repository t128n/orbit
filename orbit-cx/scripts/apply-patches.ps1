[CmdletBinding()]
param(
    [switch]$Check,
    [switch]$Reverse,
    [switch]$Am
)
$ErrorActionPreference = "Stop"

# $ErrorActionPreference does not cover native commands, so check git's exit code explicitly.
function Assert-GitSucceeded {
    param([string]$Operation)
    if ($LASTEXITCODE -ne 0) {
        throw "git $Operation failed with exit code $LASTEXITCODE"
    }
}

$scriptDir = $PSScriptRoot
$repoRoot = (Resolve-Path (Join-Path $scriptDir "../..")).Path
$patchDir = (Resolve-Path (Join-Path $scriptDir "../patches")).Path

Set-Location $repoRoot

$patches = Get-ChildItem -Path $patchDir -Filter "*.patch" | Sort-Object Name
if ($patches.Count -eq 0) {
    Write-Error "No patches found in $patchDir"
}

if ($Check) {
    Write-Host "Checking $($patches.Count) patches sequentially against index..."
    $tmpIndex = [System.IO.Path]::GetTempFileName()
    $gitDir = git rev-parse --git-dir
    Assert-GitSucceeded "rev-parse --git-dir"
    $gitDir = $gitDir.Trim()
    Copy-Item (Join-Path $gitDir "index") $tmpIndex -Force
    $env:GIT_INDEX_FILE = $tmpIndex
    try {
        foreach ($p in $patches) {
            Write-Host "  Checking $($p.Name)..."
            git apply --cached $p.FullName
            Assert-GitSucceeded "apply --cached $($p.Name)"
        }
        Write-Host "All patches apply cleanly."
    } finally {
        Remove-Item Env:GIT_INDEX_FILE -ErrorAction SilentlyContinue
        if (Test-Path $tmpIndex) { Remove-Item $tmpIndex -Force -ErrorAction SilentlyContinue }
    }
} elseif ($Reverse) {
    Write-Host "Reversing $($patches.Count) patches in reverse order..."
    for ($i = $patches.Count - 1; $i -ge 0; $i--) {
        $p = $patches[$i]
        Write-Host "  Reversing $($p.Name)..."
        git apply -R $p.FullName
        Assert-GitSucceeded "apply -R $($p.Name)"
    }
    Write-Host "All patches reversed successfully."
} elseif ($Am) {
    Write-Host "Applying $($patches.Count) patches with git am..."
    git am ($patches | ForEach-Object { $_.FullName })
    Assert-GitSucceeded "am"
    Write-Host "All patches committed successfully."
} else {
    Write-Host "Applying $($patches.Count) patches..."
    foreach ($p in $patches) {
        Write-Host "  Applying $($p.Name)..."
        git apply $p.FullName
        Assert-GitSucceeded "apply $($p.Name)"
    }
    Write-Host "All patches applied successfully."
}

param(
  [string]$Configuration = "Release"
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$outDir = Join-Path $root "bin"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$injectorSrc = Join-Path $root "injector\injector.cpp"
$hookSrc = Join-Path $root "hook\gamesaver_hook.cpp"
$injectorOut = Join-Path $outDir "gamesaver-injector.exe"
$hookOut = Join-Path $outDir "gamesaver-hook.dll"

Write-Host "Building injector..."
cl.exe /nologo /std:c++17 /EHsc /DUNICODE /D_UNICODE /Fe:$injectorOut $injectorSrc

Write-Host "Building hook DLL..."
cl.exe /nologo /std:c++17 /EHsc /LD /DUNICODE /D_UNICODE /Fe:$hookOut $hookSrc

Write-Host "Done:"
Write-Host "  $injectorOut"
Write-Host "  $hookOut"

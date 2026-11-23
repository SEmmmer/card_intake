# Builds Card Intake's Windows installer and installs it for this user, without prompts.
#
#   pwsh scripts/install-windows.ps1            build, then install
#   pwsh scripts/install-windows.ps1 -NoBuild   install the installer built last
#
# Needs the Tauri CLI, once: cargo install tauri-cli --version "^2" --locked
#
# A silent install stops the installed app if it's open, and an import with it, so this
# refuses to install while the installed Card Intake is open (a `cargo run` copy doesn't
# matter). Close it, then run again with -NoBuild.
param([switch]$NoBuild)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Card Intake'

function Get-InstalledApp {
    $entry = Get-ItemProperty $uninstallKey -ErrorAction SilentlyContinue
    if ($entry) { Join-Path $entry.InstallLocation.Trim('"') 'card_intake_desktop.exe' }
}

if (-not $NoBuild) {
    Push-Location (Join-Path $root 'apps\desktop')
    try {
        cargo tauri build --bundles nsis
        if ($LASTEXITCODE -ne 0) { throw 'The build failed.' }
    } finally {
        Pop-Location
    }
}

$setup = Get-ChildItem (Join-Path $root 'target\release\bundle\nsis\*-setup.exe') -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime | Select-Object -Last 1
if (-not $setup) { throw 'No installer in target\release\bundle\nsis. Run without -NoBuild.' }

$app = Get-InstalledApp
if ($app -and (Test-Path $app)) {
    # Windows opens a program for writing only while it isn't running, whoever started it.
    try { [IO.File]::Open($app, 'Open', 'ReadWrite', 'None').Dispose() }
    catch { throw 'Card Intake is open. Close it, then run this again with -NoBuild.' }
}

# The first install adds Start menu and desktop shortcuts; later ones leave shortcuts alone.
$flags = if ($app) { '/S', '/UPDATE' } else { '/S' }
$exit = (Start-Process $setup.FullName -ArgumentList $flags -Wait -PassThru).ExitCode
if ($exit -ne 0) { throw "The installer failed (exit code $exit)." }

$app = Get-InstalledApp
if (-not $app -or -not (Test-Path $app)) { throw 'The installer finished, but Card Intake is not installed.' }
$commit = git -C $root describe --always --dirty
"Installed Card Intake $((Get-ItemProperty $uninstallKey).DisplayVersion) ($commit): $app"

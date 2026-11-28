# Builds Card Intake's Windows installer and installs it for this user, without prompts.
#
#   pwsh scripts/install-windows.ps1            build, then install
#   pwsh scripts/install-windows.ps1 -NoBuild   install the installer built last
#
# Needs the Tauri CLI, once: cargo install tauri-cli --version "^2" --locked
#
# A silent install can't replace the app while it runs. When the installed Card Intake
# is open, this closes it first, and opens it again afterwards. While it has a job
# running (an import, a backup or Send, a delivery, a Finish: its busy.json says so),
# this waits for the job to end before closing it. A `cargo run` copy doesn't matter.
param([switch]$NoBuild, [int]$PollSeconds = 5)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Card Intake'
$busyFile = Join-Path $env:LOCALAPPDATA 'io.github.semmmer.cardintake\busy.json'

function Get-InstalledApp {
    $entry = Get-ItemProperty $uninstallKey -ErrorAction SilentlyContinue
    if ($entry) { Join-Path $entry.InstallLocation.Trim('"') 'card_intake_desktop.exe' }
}

# The installed app's running processes.
function Get-Running($app) {
    $full = [IO.Path]::GetFullPath($app)
    @(Get-Process card_intake_desktop -ErrorAction SilentlyContinue | Where-Object { $_.Path -and [IO.Path]::GetFullPath($_.Path) -eq $full })
}

# The jobs the running app has, from its busy.json; none when the file is another process's.
function Get-Jobs($running) {
    try {
        $busy = Get-Content $busyFile -Raw -ErrorAction Stop | ConvertFrom-Json
        if ($running.Id -contains [int]$busy.pid) { return @($busy.jobs) }
    } catch {}
    @()
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
$reopen = $false
if ($app -and (Test-Path $app)) {
    $running = Get-Running $app
    if ($running.Count) {
        $said = $null
        while (($jobs = Get-Jobs $running).Count) {
            $now = $jobs -join ', '
            if ($now -ne $said) { "Card Intake is busy ($now). Installing when it's done..."; $said = $now }
            Start-Sleep -Seconds $PollSeconds
            $running = Get-Running $app
            if (-not $running.Count) { break }
        }
        if ($running.Count) {
            'Closing Card Intake to install.'
            foreach ($p in $running) { [void]$p.CloseMainWindow() }
            $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
            Get-Running $app | Stop-Process -Force
            Get-Running $app | Wait-Process -Timeout 30 -ErrorAction SilentlyContinue
            $reopen = $true
        }
    }
    # Windows opens a program for writing only while it isn't running, whoever started it.
    try { [IO.File]::Open($app, 'Open', 'ReadWrite', 'None').Dispose() }
    catch { throw "Card Intake couldn't be closed for the install: $_" }
}

# The first install adds Start menu and desktop shortcuts; later ones leave shortcuts alone.
$flags = if ($app) { '/S', '/UPDATE' } else { '/S' }
$exit = (Start-Process $setup.FullName -ArgumentList $flags -Wait -PassThru).ExitCode
if ($exit -ne 0) { throw "The installer failed (exit code $exit)." }

$app = Get-InstalledApp
if (-not $app -or -not (Test-Path $app)) { throw 'The installer finished, but Card Intake is not installed.' }
$commit = git -C $root describe --always --dirty
"Installed Card Intake $((Get-ItemProperty $uninstallKey).DisplayVersion) ($commit): $app"
if ($reopen) {
    Start-Process $app
    'Opened Card Intake again.'
}

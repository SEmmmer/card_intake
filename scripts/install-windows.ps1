# Builds Card Intake's Windows installer and installs it for this user, without prompts.
#
#   pwsh scripts/install-windows.ps1            build, then install
#   pwsh scripts/install-windows.ps1 -NoBuild   install the installer built last
#
# Needs the Tauri CLI, once: cargo install tauri-cli --version "^2" --locked
#
# A silent install can't replace the app while it runs. When the installed Card Intake
# is open, this closes it first, and opens it again afterwards. While it has a job
# running, each worker holds busy.lock until it finishes. A `cargo run` copy doesn't
# participate. Earlier installs report their jobs in busy.json.
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

# Called with busy.lock already held. The previous release only reported jobs in JSON.
function Get-LegacyJobs($running) {
    try {
        $reader = [IO.StreamReader]::new([IO.File]::Open($busyFile, 'Open', 'Read', 'ReadWrite, Delete'))
        try { $busy = ConvertFrom-Json -InputObject $reader.ReadToEnd() }
        finally { $reader.Dispose() }
        if (-not $busy.pid -or $busy.jobs -isnot [array]) { return 'reading job status' }
        if ($busy.lock -eq 'busy.lock') { return } # All of this version's workers have released the lock.
        if ($running.Id -contains [int]$busy.pid) { return @($busy.jobs) }
    } catch [IO.FileNotFoundException] {
        return
    } catch {
        # An earlier version writes in place. A partial or unreadable report is not idle.
        return 'reading job status'
    }
}

function Enter-InstallLock {
    $dir = [IO.Directory]::CreateDirectory((Split-Path -Parent $busyFile))
    $path = Join-Path $dir.FullName 'busy.lock'
    $waiting = $false
    while ($true) {
        try { return [IO.File]::Open($path, 'OpenOrCreate', 'ReadWrite', 'None') }
        catch [IO.IOException] {
            # Only a sharing violation means another job or installer owns the file.
            if (($_.Exception.GetBaseException().HResult -band 0xffff) -ne 32) { throw }
        }
        if (-not $waiting) {
            Write-Host 'Waiting for Card Intake to finish before installing.'
            $waiting = $true
        }
        Start-Sleep -Seconds $PollSeconds
    }
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

$installLock = Enter-InstallLock
try {
    $app = Get-InstalledApp
    $reopen = $false
    if ($app -and (Test-Path -LiteralPath $app)) {
        $running = Get-Running $app
        $said = $null
        while ($running -and ($jobs = Get-LegacyJobs $running)) {
            $now = $jobs -join ', '
            if ($now -ne $said) { "Card Intake is busy ($now). Installing when it's done..."; $said = $now }
            Start-Sleep -Seconds $PollSeconds
            $running = Get-Running $app
        }
        if ($running) {
            'Closing Card Intake to install.'
            foreach ($p in $running) {
                if (-not $p.HasExited -and -not $p.CloseMainWindow()) { $p | Stop-Process -Force }
            }
            $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
            $remaining = @($running | Where-Object { -not $_.HasExited })
            if ($remaining) {
                $remaining | Stop-Process -Force
                $remaining | Wait-Process -Timeout 30 -ErrorAction SilentlyContinue
            }
            $reopen = $true
        }
        try { [IO.File]::Open($app, 'Open', 'ReadWrite', 'None').Dispose() }
        catch { throw "Card Intake couldn't be closed for the install: $_" }
    }

    # The first install adds Start menu and desktop shortcuts; later ones leave shortcuts alone.
    $flags = if ($app) { '/S', '/UPDATE' } else { '/S' }
    $exit = (Start-Process $setup.FullName -ArgumentList $flags -WindowStyle Hidden -Wait -PassThru).ExitCode
    if ($exit -ne 0) { throw "The installer failed (exit code $exit)." }

    $app = Get-InstalledApp
    if (-not $app -or -not (Test-Path -LiteralPath $app)) { throw 'The installer finished, but Card Intake is not installed.' }
    $commit = git -C $root describe --always --dirty
    "Installed Card Intake $((Get-ItemProperty $uninstallKey).DisplayVersion) ($commit): $app"
    if ($reopen) {
        Start-Process $app
        'Opened Card Intake again.'
    }
} finally {
    $installLock.Dispose()
}

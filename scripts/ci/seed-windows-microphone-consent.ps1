# Best-effort attempt to grant classic Win32 ("desktop") apps microphone access on
# .github/workflows/windows-app-build.yml's `e2e-best-effort` job's runner, so the
# capture-windows smoke-test binary can open a WASAPI capture stream without a GUI
# permission prompt (which a headless CI runner can never click through).
#
# THIS IS NOT A MICROSOFT-DOCUMENTED CI MECHANISM. Directly writing the
# CapabilityAccessManager ConsentStore registry keys behind Settings > Privacy &
# security > Microphone > "Let desktop apps access your microphone" is an
# undocumented, community workaround (the Windows analogue of macOS's TCC.db —
# see scripts/ci/seed-tcc-permissions.sh's own header for the same caveat there).
# Failure here is expected and handled gracefully by the caller (the
# `e2e-best-effort` job has `continue-on-error: true` precisely because of this).

$ErrorActionPreference = "Continue"

# Both HKLM (machine-wide) and HKCU (the runner process's own user) are written,
# since it's not confirmed which one this runner image's effective policy reads —
# cheap to set both rather than guess.
$paths = @(
    "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone",
    "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged",
    "HKCU:\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone",
    "HKCU:\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged"
)

foreach ($path in $paths) {
    try {
        New-Item -Path $path -Force -ErrorAction Stop | Out-Null
        Set-ItemProperty -Path $path -Name "Value" -Value "Allow" -Type String -Force -ErrorAction Stop
        Write-Host "Set microphone consent Allow at ${path}"
    } catch {
        Write-Host "Failed to seed ${path} (continuing, see this script's header comment): $_"
    }
}

Write-Host "Microphone consent seeding attempted. Actual effectiveness is only known once the smoke test runs."

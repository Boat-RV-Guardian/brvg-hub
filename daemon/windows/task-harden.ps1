# Repair the settings `schtasks /Create` silently imposes on an always-on daemon.
#
# MEASURED ON CENTRAL, 2026-08-20. A task created by schtasks and left at its defaults carries:
#
#   DisallowStartIfOnBatteries : True    the hub will not START on a battery-powered machine
#   StopIfGoingOnBatteries     : True    the hub STOPS when the machine goes on battery
#   ExecutionTimeLimit         : PT72H   Windows kills the hub after three days
#   StartWhenAvailable         : False   a missed boot trigger is never retried
#
# The second one is the product inverted. A boat or RV loses shore power and switches to battery --
# that is the exact moment the owner needs the hub watching, and Windows would shut it down.
# The third means an "always-on" service dies every 72 hours.
#
# Nobody chose any of these. They are what you get for not saying otherwise, which is why this file
# exists rather than a comment somewhere hoping the next person notices.
#
# Run as: powershell -NoProfile -ExecutionPolicy Bypass -File task-harden.ps1 -TaskName <name> -Sddl <sddl>
param(
  [Parameter(Mandatory=$true)][string]$TaskName,
  [Parameter(Mandatory=$true)][string]$Sddl
)
$ErrorActionPreference = 'Stop'
try {
  $svc = New-Object -ComObject Schedule.Service
  $svc.Connect()
  $folder = $svc.GetFolder('\')
  $def = $folder.GetTask($TaskName).Definition

  $def.Settings.DisallowStartIfOnBatteries = $false
  $def.Settings.StopIfGoingOnBatteries     = $false
  $def.Settings.ExecutionTimeLimit         = 'PT0S'   # PT0S = no limit; the daemon runs until stopped
  $def.Settings.StartWhenAvailable         = $true
  # If the hub ever exits, bring it back rather than leaving the vessel unwatched until a reboot.
  $def.Settings.RestartInterval            = 'PT1M'
  $def.Settings.RestartCount               = 3

  # 6 = TASK_CREATE_OR_UPDATE, 5 = TASK_LOGON_SERVICE_ACCOUNT. Re-registering is what commits the
  # settings; there is no in-place setter.
  $null = $folder.RegisterTaskDefinition($TaskName, $def, 6, 'SYSTEM', $null, 5)

  # SDDL LAST, deliberately: RegisterTaskDefinition rewrites the task's security descriptor, so
  # applying this first would silently undo it and standard users would stop seeing the service
  # (app #417, found live on CENTRAL).
  $folder.GetTask($TaskName).SetSecurityDescriptor($Sddl, 0)

  $check = $folder.GetTask($TaskName).Definition.Settings
  Write-Output ("task-harden: battery-start=" + (-not $check.DisallowStartIfOnBatteries) +
                " survives-power-loss=" + (-not $check.StopIfGoingOnBatteries) +
                " time-limit=" + $check.ExecutionTimeLimit)
  exit 0
} catch {
  Write-Output ("task-harden FAILED: " + $_.Exception.Message)
  exit 1
}

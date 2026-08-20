; The BRVG HUB's own Windows installer (owner ruling 2026-08-19: "the hub needs to be its own
; installer and application for both OSX and windows. you can install a hub only on both platforms
; without the UI"). It is standalone -- a boat computer can run ONLY this, with no BRVG app at all --
; and it is also the installer the app's own setup bundles and offers to launch, which is the
; owner's "separate installer triggered either during the installation process or from the app's
; menu".
;
; The hub's own choice lives HERE, not in the app's installer: one owner of the question, and the
; standalone and bundled paths cannot drift into asking it differently.
;
; SILENT USE (what the app's installer and the app's Hub screen drive):
;   brvg-hub-setup.exe /S              -> install, auto-start ON  (the sane default for a boat)
;   brvg-hub-setup.exe /S /MANUAL      -> install, auto-start OFF (installed but not at boot)
;   brvg-hub-setup.exe /S /UNINSTALL   -> remove service, binary and config
;   ... add /NOTRAY to any install to leave the notification-area monitor out
;
; EXIT CODES (the app branches on these, so do not renumber):
;   0  installed, and VERIFIED - the binary is on disk and the task is registered
;   2  a hub was already running and its program file could not be replaced (try again)
;   3  nothing usable was installed - almost always security software blocking the task or
;      quarantining the binary; see the self-check at the end of the Hub section

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!define HUB_NAME "Boat & RV Guardian Hub"
!define TASK_NAME "BoatRVGuardianHub"
; Admins+SYSTEM full control, BUILTIN\Users READ. Without the Users entry a standard user's app
; cannot SEE the task and reports "not installed" over a running hub -- found live on CENTRAL,
; 2026-08-19 (app #417). Keep this identical to hub_service.rs's TASK_SDDL.
!define TASK_SDDL "O:BAD:(A;;FA;;;BA)(A;;FA;;;SY)(A;;GR;;;BU)"

Name "${HUB_NAME}"
OutFile "brvg-hub-windows-setup.exe"
; The hub is a machine service: its binary and config live under ProgramData and its task runs as
; SYSTEM. There is no per-user variant to offer, so the installer simply requires admin.
RequestExecutionLevel admin
; NO InstallDir HERE. $PROGRAMDATA IS NOT AN NSIS CONSTANT -- see .onInit, which sets $INSTDIR the
; only way that actually works. Writing InstallDir "$PROGRAMDATA\BoatRVGuardian" compiles with a
; warning, drops the unknown token, and silently installs to \BoatRVGuardian on the current drive.
ShowInstDetails show
SetCompressor /SOLID lzma

Var AutoStart          ; "1" = start with the computer, "0" = manual only
Var Failures           ; accumulated self-check findings; non-empty ⇒ refuse to report success
Var Dlg
Var RbAuto
Var RbManual
Var ChkTray            ; the notification-area checkbox on the startup page
Var Tray               ; "1" = install the tray monitor and start it with Windows

!define MUI_ABORTWARNING
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfUninstalling
!insertmacro MUI_PAGE_WELCOME
Page custom StartupPageCreate StartupPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function SkipIfUninstalling
FunctionEnd

; The owner's second group, verbatim in intent: "Install - (Auto-Start)" / "Install - Manual Start".
; "Do not install" is this installer's Cancel -- declining to install a hub is declining to run this.
Function StartupPageCreate
  !insertmacro MUI_HEADER_TEXT "Hub startup" "Decide when the hub should run."
  nsDialogs::Create 1018
  Pop $Dlg
  ${If} $Dlg == error
    Abort
  ${EndIf}

  ${NSD_CreateLabel} 0 0 100% 26u "The hub runs in the background and carries this vehicle's local work -- gateway telemetry, local control and cloud reporting -- even when nobody is signed in."
  Pop $0

  ${NSD_CreateRadioButton} 0 34u 100% 12u "Start the hub with this computer  (recommended)"
  Pop $RbAuto
  ${NSD_CreateLabel} 12u 47u 100% 10u "For a boat or RV you leave unattended. Runs before anyone logs in."
  Pop $0

  ${NSD_CreateRadioButton} 0 63u 100% 12u "Install, but start it manually"
  Pop $RbManual
  ${NSD_CreateLabel} 12u 76u 100% 10u "For testing. The hub reports nothing until someone starts it."
  Pop $0

  ${NSD_CreateCheckbox} 0 96u 100% 12u "Show the hub in the notification area  (recommended)"
  Pop $ChkTray
  ${NSD_CreateLabel} 12u 109u 100% 18u "A small icon by the clock showing whether the hub is watching. It is also what tells you if security software removes the hub -- the installer cannot, because that happens after it finishes."
  Pop $0

  ${NSD_Check} $RbAuto
  ${If} $Tray == "1"
    ${NSD_Check} $ChkTray
  ${EndIf}
  nsDialogs::Show
FunctionEnd

Function StartupPageLeave
  ${NSD_GetState} $ChkTray $0
  ${If} $0 == ${BST_CHECKED}
    StrCpy $Tray "1"
  ${Else}
    StrCpy $Tray "0"
  ${EndIf}
  ${NSD_GetState} $RbManual $0
  ${If} $0 == ${BST_CHECKED}
    StrCpy $AutoStart "0"
  ${Else}
    StrCpy $AutoStart "1"
  ; Default ON (owner, 2026-08-20). Its whole value is being present when something goes wrong,
  ; and a monitor nobody opted into is a monitor nobody has.
  StrCpy $Tray "1"
  ${EndIf}
FunctionEnd

; Silent installs never see the page, so read the flags here and default to auto-start.
Function .onInit
  ; ProgramData, correctly. NSIS reaches it as $APPDATA under the "all users" shell context -- there
  ; is no $PROGRAMDATA constant. The context is set once here and left set: this installer is
  ; machine-wide by definition (RequestExecutionLevel admin, a SYSTEM task), so every shell folder
  ; it touches should be the machine-wide one.
  SetShellVarContext all
  StrCpy $INSTDIR "$APPDATA\BoatRVGuardian"

  StrCpy $AutoStart "1"
  ${GetParameters} $R0
  ClearErrors
  ${GetOptions} $R0 "/NOTRAY" $R1
  ${IfNot} ${Errors}
    StrCpy $Tray "0"
  ${EndIf}
  ClearErrors
  ${GetOptions} $R0 "/MANUAL" $R1
  ${IfNot} ${Errors}
    StrCpy $AutoStart "0"
  ${EndIf}
  ClearErrors
  ${GetOptions} $R0 "/UNINSTALL" $R1
  ${IfNot} ${Errors}
    Call RemoveEverything
    SetErrorLevel 0
    Quit
  ${EndIf}
FunctionEnd

Section "Hub" SecHub
  ; UPGRADING OVER A RUNNING HUB IS THE NORMAL CASE, NOT THE EDGE CASE.
  ; The app installs a hub at this same path under this same task name, and re-running this
  ; installer is how a repair or an upgrade happens. A running hub holds bin\brvg-hub.exe open, so
  ; File cannot overwrite it and the install fails at its very first instruction.
  ;
  ; Measured on CENTRAL, 2026-08-19, with the task in state Running:
  ;   [System.IO.File]::OpenWrite("C:\ProgramData\BoatRVGuardian\bin\brvg-hub.exe")
  ;   -> "The process cannot access the file ... because it is being used by another process."
  DetailPrint "Stopping any hub that is already running..."
  nsExec::ExecToLog 'schtasks /End /TN "${TASK_NAME}"'
  Pop $0

  ; /End only reaches a process the scheduler owns. A hub started by hand (brvg-hub --hub), or
  ; orphaned when a task died badly, holds exactly the same lock and would still block the write.
  ; So follow up unconditionally -- failure here is fine and expected when nothing is running.
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub.exe /T'
  Pop $0

  ; Windows releases the file handle when the process is reaped, which is not synchronous with
  ; taskkill returning.
  Sleep 1500

  SetOutPath "$INSTDIR\bin"
  ; `try` so a still-locked binary sets the error flag instead of throwing NSIS's own abort/retry
  ; dialog, which is unanswerable during a /S silent install driven by the app.
  SetOverwrite try
  File "brvg-hub.exe"
  IfErrors hub_locked hub_written
  hub_locked:
    DetailPrint "ERROR: the hub program file is still in use and could not be replaced."
    SetErrorLevel 2
    Abort "A hub is still running and its program file could not be replaced. Stop it and run this installer again."
  hub_written:
  SetOverwrite on

  DetailPrint "Registering the hub's background service..."
  ; ONSTART under SYSTEM: the boot-before-login shape. /F so a re-install repairs rather than fails.
  ;
  ; ⚠️ /TR TAKES A BARE PATH. DO NOT WRAP IT IN QUOTES.
  ; It reads like a command line and it is not: schtasks puts the value straight into the task's
  ; <Command> element, which Task Scheduler treats as a literal filename. Quoting it produced
  ;   <Command>"C:\ProgramData\BoatRVGuardian\bin\brvg-hub.exe"</Command>
  ; and Windows then looked for a file whose name BEGINS with a quote character. Measured on
  ; CENTRAL 2026-08-20: `schtasks /Run` -> "The parameter is incorrect", the COM API -> "Value does
  ; not fall within the expected range", and the task never ran once. The same task created with a
  ; bare path started the hub instantly. This path lives under ProgramData and has no spaces.
  nsExec::ExecToLog 'schtasks /Create /F /TN "${TASK_NAME}" /SC ONSTART /RU SYSTEM /RL HIGHEST /TR $INSTDIR\bin\brvg-hub.exe'
  Pop $0
  ${If} $0 != 0
    DetailPrint "WARNING: could not register the service (schtasks exit $0)."
  ${EndIf}

  ; Repair the settings schtasks imposes by default, then apply the SDDL. Both live in a .ps1
  ; rather than inline here because the last inline PowerShell one-liner needed nine backslashes to
  ; express one, and this one has to set six properties and re-register the task.
  ;
  ; What it fixes, all measured on CENTRAL 2026-08-20 and none of them anyone's decision:
  ;   StopIfGoingOnBatteries=True   -- Windows STOPS the hub when the machine goes on battery.
  ;                                    On a boat that is the moment shore power drops, which is
  ;                                    precisely when the owner needs it watching.
  ;   DisallowStartIfOnBatteries=True -- and it will not start there in the first place.
  ;   ExecutionTimeLimit=PT72H      -- an "always-on" service killed every three days.
  File "task-harden.ps1"
  nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\bin\task-harden.ps1" -TaskName "${TASK_NAME}" -Sddl "${TASK_SDDL}"'
  Pop $0
  ${If} $0 != 0
    DetailPrint "WARNING: could not harden the service settings (exit $0)."
  ${EndIf}
  Delete "$INSTDIR\bin\task-harden.ps1"

  ${If} $AutoStart == "1"
    DetailPrint "The hub will start with this computer."
    nsExec::ExecToLog 'schtasks /Run /TN "${TASK_NAME}"'
    Pop $0
  ${Else}
    DetailPrint "Installed. The hub will only start when told to."
    nsExec::ExecToLog 'schtasks /Change /TN "${TASK_NAME}" /Disable'
    Pop $0
  ${EndIf}

  ; ---- VERIFY OUR OWN WORK BEFORE CLAIMING SUCCESS -------------------------------------------
  ;
  ; MEASURED ON CENTRAL, 2026-08-19. Sophos Endpoint Defense flagged the schtasks call above as
  ; Persist_6a (MITRE T1053.005 - Scheduled Task persistence), blocked the task, and QUARANTINED
  ; bin\brvg-hub.exe about 14 seconds later. This installer exited 0 the entire time.
  ;
  ; That is the worst possible outcome. A visible failure sends someone looking; a SILENT one
  ; leaves the app's setup screen sitting on "install the service" forever with nothing to read,
  ; on a machine where the hub will never run. An unsigned installer invoking schtasks to create a
  ; SYSTEM boot task is a textbook persistence pattern, so this is not exotic -- Sophos Home is
  ; consumer software, and any behavioural endpoint agent watches for it.
  ;
  ; So do not trust the exit codes of tools an endpoint agent can neutralise underneath us. Look
  ; at the disk and the task store and report what is ACTUALLY there.
  StrCpy $Failures ""

  IfFileExists "$INSTDIR\bin\brvg-hub.exe" check_task 0
    StrCpy $Failures "$Failures$\r$\n  - the hub program file is missing from $INSTDIR\bin"
  check_task:

  nsExec::ExecToLog 'schtasks /Query /TN "${TASK_NAME}"'
  Pop $0
  ${If} $0 != 0
    StrCpy $Failures "$Failures$\r$\n  - the background service was not registered"
  ${EndIf}

  ; AND THAT IT ACTUALLY STARTED. Existence is not running, and the difference is not academic:
  ; on CENTRAL 2026-08-20 the binary was on disk and the task was registered and queryable -- this
  ; check passed -- while the task could not launch anything at all, because its <Command> carried
  ; literal quotes. The installer said "installed" about a hub that had never run and could not.
  ;
  ; Only when auto-start was chosen. "Install, but start it manually" means not running is correct.
  ; Up to 15s: the daemon binds a socket and reads its config, and a slow disk should not fail an
  ; install that is fine.
  ${If} $AutoStart == "1"
    nsExec::ExecToLog 'powershell -NoProfile -Command "for ($$i=0; $$i -lt 15; $$i++) { if (Get-Process brvg-hub -ErrorAction SilentlyContinue) { exit 0 }; Start-Sleep -Seconds 1 }; exit 1"'
    Pop $0
    ${If} $0 != 0
      StrCpy $Failures "$Failures$\r$\n  - the hub was installed but did not start"
    ${EndIf}
  ${EndIf}

  ${If} $Failures != ""
    DetailPrint "INSTALL FAILED -- see the message below."
    ; Exit code 3 = installed nothing usable. Distinct from 2 (could not replace a running hub) so
    ; the app can tell "try again later" apart from "something on this machine refused us".
    SetErrorLevel 3
    ; The owner's ruling, 2026-08-19: no code-signing certificate. Fail loudly and TELL THE USER
    ; HOW TO FIX IT AND WHY. That makes this text a product feature, not an error string -- it is
    ; the entire remedy for a blocked install, so it explains the cause before the steps.
    ;
    ; "Windows Defender allows it" is stated on evidence, not reassurance: measured on a clean
    ; Defender-only Win 11 machine 2026-08-19 (Proxmox VM 107) -- installed cleanly, hub answered,
    ; zero detections. Naming that spares the user hunting through Defender when their actual
    ; blocker is a third-party product.
    Abort "The hub was NOT installed.$\r$\n$Failures$\r$\n$\r$\nWHY THIS HAPPENS$\r$\nThe hub watches your boat or RV while nobody is aboard, so it has to start with the computer, before anyone signs in. Security software cannot tell that apart from a program trying to hide itself, so some products block it. Windows Defender allows it -- if you are seeing this, it is usually a third-party antivirus.$\r$\n$\r$\nHOW TO FIX IT$\r$\n1. Open your antivirus or endpoint protection.$\r$\n2. Find its quarantine, history, or recent events, and look for an item named brvg-hub or a blocked scheduled task.$\r$\n3. Choose Allow, Restore, or Trust for that item.$\r$\n4. Run this installer again.$\r$\n$\r$\nIf you would rather not allow it, the hub simply will not run on this computer. Nothing else about Boat & RV Guardian is affected -- the app still works, it just cannot monitor while you are away."
  ${EndIf}

  ; ---- THE NOTIFICATION-AREA MONITOR -----------------------------------------------------------
  ; Optional, default on (owner, 2026-08-20). It exists because THIS INSTALLER CANNOT REPORT ITS
  ; OWN LATE FAILURES: measured on CENTRAL, a /S install returned 0, passed its self-check, and had
  ; its binary and task removed by security software ~10s after the process exited. Something
  ; resident is the only thing that can tell the user about that.
  ${If} $Tray == "1"
    SetOutPath "$INSTDIR\bin"
    File "brvg-hub-tray.exe"

    ; HKLM, not HKCU -- owner ruling: "for the taskbar, all users like the hub". Every account that
    ; logs in gets the icon, which matches a hub that is machine-wide.
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "BoatRVGuardianHubTray" '"$INSTDIR\bin\brvg-hub-tray.exe"'

    ; Start it now rather than making the user log out to see it. VIA EXPLORER ON PURPOSE: this
    ; installer is elevated, and a child of an elevated process is elevated too -- an elevated tray
    ; icon behaves badly in a normal user's session (UIPI). explorer.exe runs as the logged-in user,
    ; so handing it the path launches the monitor DE-ELEVATED, which is what it should be. If this
    ; fails for any reason it is cosmetic: the Run key above starts it at the next sign-in anyway.
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\bin\brvg-hub-tray.exe"'
  ${Else}
    ; Declining on a re-install must actually REMOVE it, not just skip adding it.
    DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "BoatRVGuardianHubTray"
    nsExec::ExecToLog 'taskkill /F /IM brvg-hub-tray.exe'
    Pop $0
    Delete "$INSTDIR\bin\brvg-hub-tray.exe"
  ${EndIf}

  WriteUninstaller "$INSTDIR\uninstall-hub.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\BoatRVGuardianHub" "DisplayName" "${HUB_NAME}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\BoatRVGuardianHub" "UninstallString" '"$INSTDIR\uninstall-hub.exe"'
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\BoatRVGuardianHub" "Publisher" "SC4 Tech"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\BoatRVGuardianHub" "InstallLocation" "$INSTDIR"
SectionEnd

; Shared by the uninstaller and /UNINSTALL. Removes the service, the binary AND the hub's config --
; the config holds a cloud credential, and leaving it behind with nothing able to clean it up is
; worse than removing it. Revoking that credential cloud-side is the app's job, separately.
Function RemoveEverything
  ; $APPDATA under the all-users context set in .onInit == C:\ProgramData. This function previously
  ; used $PROGRAMDATA, which does not exist: every path below resolved to \BoatRVGuardian\... on the
  ; current drive, so `/S /UNINSTALL` -- the path the APP drives -- reported success and deleted
  ; NOTHING. hub.json holds a cloud credential, which makes that a leak rather than untidiness.
  SetShellVarContext all
  nsExec::ExecToLog 'schtasks /End /TN "${TASK_NAME}"'
  Pop $0
  nsExec::ExecToLog 'schtasks /Delete /F /TN "${TASK_NAME}"'
  Pop $0
  ; /End only reaches a scheduler-owned process; anything else keeps the binary locked and RMDir
  ; would silently leave it behind.
  ; The tray monitor rides along with the hub -- a leftover Run key pointing at a deleted file, or
  ; a running monitor polling a hub that no longer exists, are both worse than nothing.
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub-tray.exe'
  Pop $0
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "BoatRVGuardianHubTray"
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub.exe /T'
  Pop $0
  Sleep 1500
  RMDir /r "$APPDATA\BoatRVGuardian\bin"
  Delete "$APPDATA\BoatRVGuardian\hub.json"
  Delete "$APPDATA\BoatRVGuardian\uninstall-hub.exe"
  RMDir "$APPDATA\BoatRVGuardian"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\BoatRVGuardianHub"
FunctionEnd

Section "Uninstall"
  nsExec::ExecToLog 'schtasks /End /TN "${TASK_NAME}"'
  Pop $0
  nsExec::ExecToLog 'schtasks /Delete /F /TN "${TASK_NAME}"'
  Pop $0
  ; Same reason as RemoveEverything: a hub not owned by the scheduler holds bin\brvg-hub.exe open,
  ; and RMDir would quietly skip it, leaving a working hub behind after a "successful" uninstall.
  ; The tray monitor rides along with the hub -- a leftover Run key pointing at a deleted file, or
  ; a running monitor polling a hub that no longer exists, are both worse than nothing.
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub-tray.exe'
  Pop $0
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "BoatRVGuardianHubTray"
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub.exe /T'
  Pop $0
  Sleep 1500
  RMDir /r "$INSTDIR\bin"
  Delete "$INSTDIR\hub.json"
  Delete "$INSTDIR\uninstall-hub.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\BoatRVGuardianHub"
SectionEnd

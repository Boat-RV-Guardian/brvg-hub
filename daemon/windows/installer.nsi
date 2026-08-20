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
; TEMPORARY — throwaway branch proving -WX turns warning 6000 into a build failure.
DetailPrint "$NOTAVARIABLE"
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
Var Dlg
Var RbAuto
Var RbManual

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

  ${NSD_Check} $RbAuto
  nsDialogs::Show
FunctionEnd

Function StartupPageLeave
  ${NSD_GetState} $RbManual $0
  ${If} $0 == ${BST_CHECKED}
    StrCpy $AutoStart "0"
  ${Else}
    StrCpy $AutoStart "1"
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
  nsExec::ExecToLog 'schtasks /Create /F /TN "${TASK_NAME}" /SC ONSTART /RU SYSTEM /RL HIGHEST /TR "\"$INSTDIR\bin\brvg-hub.exe\""'
  Pop $0
  ${If} $0 != 0
    DetailPrint "WARNING: could not register the service (schtasks exit $0)."
  ${EndIf}

  ; Let every signed-in account SEE the service. See TASK_SDDL above.
  nsExec::ExecToLog 'powershell -NoProfile -Command "$$s = New-Object -ComObject Schedule.Service; $$s.Connect(); $$s.GetFolder(\"\\\").GetTask(\"${TASK_NAME}\").SetSecurityDescriptor(\"${TASK_SDDL}\", 0)"'
  Pop $0

  ${If} $AutoStart == "1"
    DetailPrint "The hub will start with this computer."
    nsExec::ExecToLog 'schtasks /Run /TN "${TASK_NAME}"'
    Pop $0
  ${Else}
    DetailPrint "Installed. The hub will only start when told to."
    nsExec::ExecToLog 'schtasks /Change /TN "${TASK_NAME}" /Disable'
    Pop $0
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
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub.exe /T'
  Pop $0
  Sleep 1500
  RMDir /r "$INSTDIR\bin"
  Delete "$INSTDIR\hub.json"
  Delete "$INSTDIR\uninstall-hub.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\BoatRVGuardianHub"
SectionEnd

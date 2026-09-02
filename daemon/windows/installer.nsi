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
;   0  installed, and VERIFIED - the binary is on disk and the service is registered
;   2  a hub was already running and its program file could not be replaced (try again)
;   3  nothing usable was installed - almost always security software blocking the service or
;      quarantining the binary; see the self-check at the end of the Hub section

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!define HUB_NAME "Boat & RV Guardian Hub"

; THE HUB IS A REAL WINDOWS SERVICE, NOT A SCHEDULED TASK (owner ruling 2026-08-20: "i want you to
; build it as a service no matter what, i don't like it as a scheduled task").
;
; ⚠️ THIS NAME IS A THREE-WAY CONTRACT and a mismatch is silent. It must equal:
;   * SERVICE_NAME in daemon/src/win_service.rs  -- the SCM starts the binary, and the binary
;     registers its control handler under this exact string. Wrong here and the service starts,
;     never reports RUNNING, and the SCM kills it.
;   * SERVICE_NAME in the APP's dashboard/src-tauri/src/hub_service.rs -- that is what `sc query`s
;     to draw the Hub screen. Wrong here and the app manages a service the daemon never answers.
!define SERVICE_NAME "DockNeighborHub"
!define SERVICE_DESC "Carries this vehicle's local work - gateway telemetry, local control and cloud reporting - even when nobody is signed in."

; The scheduled task this installer used to create, kept ONLY so an upgrade removes it. A machine
; that still carries it would otherwise end up with two persistence entries fighting over port
; 8722. New installs never create a task.
;
; ⚠️ THIS NAMES HISTORY, SO IT DOES NOT MOVE WITH ${SERVICE_NAME}. No machine ever carried a
; scheduled task called "DockNeighborHub"; renaming this turns the cleanup into a silent no-op and
; leaves exactly the two-persistence-entries state it exists to prevent.
!define LEGACY_TASK_NAME "BoatRVGuardianHub"

; And the SERVICE by its former name -- the same reasoning one layer up. Every hub installed before
; 2026-09-02 runs as "BoatRVGuardianHub"; installing ${SERVICE_NAME} over it without removing it
; leaves TWO services racing for port 8722, and the survivor is the old binary still reporting to
; the retired api.boatrvguardian.com. Silenced everywhere -- a fresh machine never had one.
!define LEGACY_SERVICE_NAME "BoatRVGuardianHub"

; The shared data directory, under $APPDATA in all-users context (== C:\ProgramData).
; ⚠️ MUST equal DIR_NAME in daemon/src/hub_config.rs and the app's hub_bin_path(); the legacy one is
; kept so an upgrade can clear the old tree out. Renamed with the service, 2026-09-02.
!define DATA_DIR_NAME "DockNeighbor"
!define LEGACY_DATA_DIR_NAME "BoatRVGuardian"

Name "${HUB_NAME}"
OutFile "brvg-hub-windows-setup.exe"
; The hub is a machine service: its binary and config live under ProgramData and its service runs as
; SYSTEM. There is no per-user variant to offer, so the installer simply requires admin.
RequestExecutionLevel admin
; NO InstallDir HERE. $PROGRAMDATA IS NOT AN NSIS CONSTANT -- see .onInit, which sets $INSTDIR the
; only way that actually works. Writing InstallDir "$PROGRAMDATA\${DATA_DIR_NAME}" compiles with a
; warning, drops the unknown token, and silently installs to \DockNeighbor on the current drive.
ShowInstDetails show
SetCompressor /SOLID lzma

Var AutoStart          ; "1" = start with the computer, "0" = manual only
Var Failures           ; accumulated self-check findings; non-empty ⇒ refuse to report success
Var Dlg
Var RbAuto
Var RbManual
Var ChkTray            ; the notification-area checkbox on the startup page
Var Tray               ; "1" = install the tray monitor and start it with Windows
Var StartMode          ; "auto" or "demand" -- the service start type, set at creation

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
  ${EndIf}
  ; The tray default (ON unless /NOTRAY, owner 2026-08-20) now lives in .onInit, which is also the
  ; only place a SILENT install can honour it. It must NOT be re-asserted here: this function ran
  ; AFTER reading the checkbox above, so forcing $Tray back to "1" in the auto-start branch (as it
  ; used to) silently ignored a user who unchecked the tray but left auto-start on — the common case.
FunctionEnd

; Silent installs never see the page, so read the flags here and default to auto-start.
Function .onInit
  ; ProgramData, correctly. NSIS reaches it as $APPDATA under the "all users" shell context -- there
  ; is no $PROGRAMDATA constant. The context is set once here and left set: this installer is
  ; machine-wide by definition (RequestExecutionLevel admin, a LocalSystem service), so every shell folder
  ; it touches should be the machine-wide one.
  SetShellVarContext all
  StrCpy $INSTDIR "$APPDATA\${DATA_DIR_NAME}"

  StrCpy $AutoStart "1"
  ; Default the tray ON, like AutoStart — the docs (top of file) say the monitor ships unless /NOTRAY
  ; is passed. Without this default, a SILENT install (which never shows the startup page that would
  ; otherwise set $Tray) left it empty, so the "$Tray == 1" section was skipped and silent installs
  ; got no tray at all — the opposite of the documented default. The interactive page still overrides
  ; this from the checkbox; /NOTRAY below still turns it off.
  StrCpy $Tray "1"
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
  ; The app installs a hub at this same path under this same SERVICE name, and re-running this
  ; installer is how a repair or an upgrade happens. A running hub holds bin\brvg-hub.exe open, so
  ; File cannot overwrite it and the install fails at its very first instruction.
  ;
  ; Measured on CENTRAL, 2026-08-19, with the task in state Running:
  ;   [System.IO.File]::OpenWrite("C:\ProgramData\DockNeighbor\bin\brvg-hub.exe")
  ;   -> "The process cannot access the file ... because it is being used by another process."
  DetailPrint "Stopping any hub that is already running..."
  nsExec::ExecToLog 'sc.exe stop "${SERVICE_NAME}"'
  Pop $0
  ; And BOTH legacy persistence entries, on a machine upgrading from an older hub: the service
  ; under its former name (pre-2026-09-02) and the scheduled task from before the service existed.
  ; Each holds the same binary open, and each left behind would give the machine two things trying
  ; to run one hub on port 8722 -- with the old one still reporting to the retired API host.
  nsExec::ExecToLog 'sc.exe stop "${LEGACY_SERVICE_NAME}"'
  Pop $0
  nsExec::ExecToLog 'schtasks /End /TN "${LEGACY_TASK_NAME}"'
  Pop $0

  ; `sc stop` only reaches a process the SCM owns. A hub started by hand (brvg-hub --hub), or
  ; orphaned when a service died badly, holds exactly the same lock and would still block the write.
  ; So follow up unconditionally -- failure here is fine and expected when nothing is running.
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub.exe /T'
  Pop $0

  ; Windows releases the file handle when the process is reaped, which is not synchronous with
  ; taskkill returning.
  Sleep 1500

  ; DELETE BEFORE CREATE, because `sc create` on an existing name fails with 1073 rather than
  ; repairing it -- there is no /F equivalent. A re-install and an upgrade both land here, so this
  ; is the normal path, not the edge case. `sc delete` on a service that is still RUNNING only
  ; MARKS it for deletion and the name stays taken, which is why the stop and the taskkill above
  ; come first and why the wait below is not optional.
  nsExec::ExecToLog 'sc.exe delete "${SERVICE_NAME}"'
  Pop $0
  nsExec::ExecToLog 'sc.exe delete "${LEGACY_SERVICE_NAME}"'
  Pop $0
  nsExec::ExecToLog 'schtasks /Delete /F /TN "${LEGACY_TASK_NAME}"'
  Pop $0
  ; The old identity's registry entries go with it, or Add/Remove Programs keeps offering to
  ; uninstall a hub that no longer exists and a stale Run value points the tray at a deleted path.
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_SERVICE_NAME}"
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${LEGACY_SERVICE_NAME}Tray"
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
  ;
  ; ⚠️ THIS WAS `schtasks /Create /SC ONSTART` UNTIL NOW, AND THE CHANGE IS THE WHOLE POINT OF THIS
  ; SECTION. Measured on CENTRAL 2026-08-19: Sophos Endpoint Defense flagged the schtasks call as
  ; Persist_6a (MITRE T1053.005 - Scheduled Task persistence), blocked the task, and quarantined
  ; bin\brvg-hub.exe about 14 seconds later. It keys on the SCHEDULED-TASK PATTERN, not on the
  ; process chain -- proven by re-running the same install as a real service under the same active
  ; Sophos, which went UNFLAGGED. A service is T1543.003, a different technique the same product
  ; did not act on.
  ;
  ; The app's own installer (dashboard/src-tauri/src/hub_service.rs) moved to a service on
  ; 2026-08-20. This script did not, which left the two disagreeing about what a hub even IS: a
  ; machine installed from THIS file got a task, and the app -- which queries `sc` -- reported no
  ; hub installed over a hub that was running fine. Same string, two namespaces, nothing to see.
  ;
  ; THE SPACE AFTER EACH `=` IS REQUIRED. `sc.exe` parses `binPath= value` as one option; writing
  ; `binPath=value` silently misparses. This is sc's syntax, not a typo.
  ;
  ; The `--service` flag is what makes the binary start an SCM control dispatcher instead of
  ; running as a plain foreground hub (daemon/src/main.rs). Without it the SCM starts the process,
  ; waits for a service that never reports in, and kills it -- so the flag is part of the contract,
  ; not a convenience.
  ;
  ; ⚠️ THE `\$\"` IS A DOUBLE ESCAPE AND BOTH HALVES ARE NECESSARY. The registered binPath must be
  ;   "C:\ProgramData\DockNeighbor\bin\brvg-hub.exe" --service
  ; quotes included, so that the SCM splits the program from its argument. Getting there needs a
  ; literal BACKSLASH-QUOTE on sc.exe's command line, because sc's value is itself wrapped in quotes
  ; and the C runtime's argv parser would otherwise eat the inner pair -- `binPath= ""path" --service"`
  ; collapses to `path --service` with no quotes at all. In NSIS, `\` is the backslash and `$\"` is
  ; the quote. The app's installer reaches the same string a different way (PowerShell New-Service,
  ; which needs no escaping), and the two MUST agree.
  ;
  ; This survives on a path with no spaces even when it is wrong, which is exactly why it is spelled
  ; out here: $INSTDIR is C:\ProgramData\DockNeighbor today, and the day it is not, an unquoted
  ; binPath registers a service that can never start.
  ${If} $AutoStart == "1"
    StrCpy $StartMode "auto"
  ${Else}
    StrCpy $StartMode "demand"
  ${EndIf}
  nsExec::ExecToLog 'sc.exe create "${SERVICE_NAME}" binPath= "\$\"$INSTDIR\bin\brvg-hub.exe\$\" --service" start= $StartMode DisplayName= "${HUB_NAME}"'
  Pop $0
  ${If} $0 != 0
    DetailPrint "WARNING: could not register the service (sc create exit $0)."
  ${EndIf}

  ; The description is a separate call -- `sc create` has no parameter for it. Cosmetic, so a
  ; failure is not collected: it decides what services.msc shows, never whether the hub runs.
  nsExec::ExecToLog 'sc.exe description "${SERVICE_NAME}" "${SERVICE_DESC}"'
  Pop $0

  ; RESTART ON CRASH -- the one thing the scheduled task genuinely gave us for free and a service
  ; does not. A boat's hub is unattended for weeks; a panic that leaves it dead until someone walks
  ; aboard is the failure that matters. Three restarts a minute apart, with the count resetting
  ; each day, so a repeatedly-crashing build still backs off instead of spinning forever.
  ; ⚠️ Same spacing rule as above, and `actions=` takes slash-separated pairs in milliseconds.
  ; Identical to what the app's installer applies -- the two must not drift.
  nsExec::ExecToLog 'sc.exe failure "${SERVICE_NAME}" reset= 86400 actions= restart/60000/restart/60000/restart/60000'
  Pop $0

  ; ⚠️ THE SCHEDULED TASK'S ENTIRE HARDENING PASS IS GONE WITH THE TASK, AND NOTHING REPLACED IT
  ; BECAUSE NOTHING NEEDS TO. task-harden.ps1 existed to undo defaults `schtasks /Create` imposes,
  ; all measured on CENTRAL 2026-08-20 and none of them anyone's decision:
  ;   StopIfGoingOnBatteries=True     -- Windows STOPPED the hub when the machine went on battery,
  ;                                      i.e. the moment shore power drops, which is precisely when
  ;                                      the owner needs it watching.
  ;   DisallowStartIfOnBatteries=True -- and it would not start there in the first place.
  ;   ExecutionTimeLimit=PT72H        -- an "always-on" service killed every three days.
  ; A Windows service has no battery policy and no execution time limit. These are task concepts,
  ; so the whole class of bug is removed rather than re-fixed.
  ;
  ; TASK_SDDL is gone for the same kind of reason. The task's default security descriptor hid it
  ; from standard users, so an unelevated app reported "not installed" over a running hub (app
  ; #417) and we had to widen it by hand. The SCM's default descriptor already grants every
  ; authenticated account SERVICE_QUERY_STATUS, so `sc query` works unelevated with nothing added.
  ; ⚠️ STILL WORTH ONE BENCH CHECK AS A GENUINE STANDARD USER -- it is the exact surface that bit
  ; us on the task, and "the default should grant it" is a claim until a non-admin account has
  ; actually seen this service on a real box.

  ${If} $AutoStart == "1"
    DetailPrint "The hub will start with this computer."
    nsExec::ExecToLog 'sc.exe start "${SERVICE_NAME}"'
    Pop $0
  ${Else}
    ; `start= demand` above already did this. Unlike the task -- where /Disable was a second,
    ; separate call after /Create -- the service's start type is set at creation, so there is
    ; nothing to turn off here.
    DetailPrint "Installed. The hub will only start when told to."
  ${EndIf}

  ; ---- VERIFY OUR OWN WORK BEFORE CLAIMING SUCCESS -------------------------------------------
  ;
  ; MEASURED ON CENTRAL, 2026-08-19, WHEN THIS SECTION STILL CREATED A SCHEDULED TASK. Sophos
  ; Endpoint Defense flagged the schtasks call as Persist_6a (MITRE T1053.005), blocked the task,
  ; and QUARANTINED bin\brvg-hub.exe about 14 seconds later. This installer exited 0 the entire
  ; time.
  ;
  ; That is the worst possible outcome. A visible failure sends someone looking; a SILENT one
  ; leaves the app's setup screen sitting on "install the service" forever with nothing to read,
  ; on a machine where the hub will never run.
  ;
  ; ⚠️ THE SERVICE WENT UNFLAGGED BY THAT SAME ACTIVE SOPHOS, BUT THAT DOES NOT RETIRE THIS CHECK,
  ; AND DO NOT LET ANYONE ARGUE THAT IT DOES. One product, one version, one machine, on one
  ; technique. This installer is still unsigned and still registers a SYSTEM-executed persistence
  ; entry, which is a thing behavioural endpoint agents are built to notice however it is spelled.
  ; So do not trust the exit codes of tools an endpoint agent can neutralise underneath us. Look at
  ; the disk and the SCM and report what is ACTUALLY there.
  StrCpy $Failures ""

  IfFileExists "$INSTDIR\bin\brvg-hub.exe" check_service 0
    StrCpy $Failures "$Failures$\r$\n  - the hub program file is missing from $INSTDIR\bin"
  check_service:

  nsExec::ExecToLog 'sc.exe query "${SERVICE_NAME}"'
  Pop $0
  ${If} $0 != 0
    StrCpy $Failures "$Failures$\r$\n  - the background service was not registered"
  ${EndIf}

  ; AND THAT IT ACTUALLY STARTED. Existence is not running, and the difference is not academic:
  ; on CENTRAL 2026-08-20 the binary was on disk and the persistence entry was registered and
  ; queryable -- this check passed -- while it could not launch anything at all. The installer said
  ; "installed" about a hub that had never run and could not.
  ;
  ; A service adds a second way to be registered-but-dead that a task did not have: if the binary
  ; is started WITHOUT `--service` it never reports to the SCM, so `sc create` succeeds, `sc start`
  ; times out at 30s with error 1053, and `sc query` still shows the service. Watching for the
  ; PROCESS, not the SCM's opinion, is what catches that.
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
    Abort "The hub was NOT installed.$\r$\n$Failures$\r$\n$\r$\nWHY THIS HAPPENS$\r$\nThe hub watches your boat or RV while nobody is aboard, so it has to start with the computer, before anyone signs in. Security software cannot tell that apart from a program trying to hide itself, so some products block it. Windows Defender allows it -- if you are seeing this, it is usually a third-party antivirus.$\r$\n$\r$\nHOW TO FIX IT$\r$\n1. Open your antivirus or endpoint protection.$\r$\n2. Find its quarantine, history, or recent events, and look for an item named brvg-hub or a blocked service.$\r$\n3. Choose Allow, Restore, or Trust for that item.$\r$\n4. Run this installer again.$\r$\n$\r$\nIf you would rather not allow it, the hub simply will not run on this computer. Nothing else about Boat & RV Guardian is affected -- the app still works, it just cannot monitor while you are away."
  ${EndIf}

  ; ---- THE NOTIFICATION-AREA MONITOR -----------------------------------------------------------
  ; Optional, default on (owner, 2026-08-20). It exists because THIS INSTALLER CANNOT REPORT ITS
  ; OWN LATE FAILURES: measured on CENTRAL, a /S install returned 0, passed its self-check, and had
  ; its binary and persistence entry removed by security software ~10s after the process exited. Something
  ; resident is the only thing that can tell the user about that.
  ${If} $Tray == "1"
    SetOutPath "$INSTDIR\bin"
    File "brvg-hub-tray.exe"

    ; HKLM, not HKCU -- owner ruling: "for the taskbar, all users like the hub". Every account that
    ; logs in gets the icon, which matches a hub that is machine-wide.
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${SERVICE_NAME}Tray" '"$INSTDIR\bin\brvg-hub-tray.exe"'

    ; Start it now rather than making the user log out to see it. VIA EXPLORER ON PURPOSE: this
    ; installer is elevated, and a child of an elevated process is elevated too -- an elevated tray
    ; icon behaves badly in a normal user's session (UIPI). explorer.exe runs as the logged-in user,
    ; so handing it the path launches the monitor DE-ELEVATED, which is what it should be. If this
    ; fails for any reason it is cosmetic: the Run key above starts it at the next sign-in anyway.
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\bin\brvg-hub-tray.exe"'
  ${Else}
    ; Declining on a re-install must actually REMOVE it, not just skip adding it.
    DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${SERVICE_NAME}Tray"
    DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${LEGACY_SERVICE_NAME}Tray"
    nsExec::ExecToLog 'taskkill /F /IM brvg-hub-tray.exe'
    Pop $0
    Delete "$INSTDIR\bin\brvg-hub-tray.exe"
  ${EndIf}

  WriteUninstaller "$INSTDIR\uninstall-hub.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${SERVICE_NAME}" "DisplayName" "${HUB_NAME}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${SERVICE_NAME}" "UninstallString" '"$INSTDIR\uninstall-hub.exe"'
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${SERVICE_NAME}" "Publisher" "SC4 Tech"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${SERVICE_NAME}" "InstallLocation" "$INSTDIR"
SectionEnd

; Shared by the uninstaller and /UNINSTALL. Removes the service, the binary AND the hub's config --
; the config holds a cloud credential, and leaving it behind with nothing able to clean it up is
; worse than removing it. Revoking that credential cloud-side is the app's job, separately.
Function RemoveEverything
  ; $APPDATA under the all-users context set in .onInit == C:\ProgramData. This function previously
  ; used $PROGRAMDATA, which does not exist: every path below resolved to \DockNeighbor\... on the
  ; current drive, so `/S /UNINSTALL` -- the path the APP drives -- reported success and deleted
  ; NOTHING. hub.json holds a cloud credential, which makes that a leak rather than untidiness.
  SetShellVarContext all
  nsExec::ExecToLog 'sc.exe stop "${SERVICE_NAME}"'
  Pop $0
  ; A service still RUNNING is only MARKED for deletion, so stop it, give it a moment, then delete.
  Sleep 1500
  nsExec::ExecToLog 'sc.exe delete "${SERVICE_NAME}"'
  Pop $0
  ; Both legacy persistence entries too -- the service under its former name (a hub installed
  ; before 2026-09-02) and the scheduled task (before 2026-08-27). An uninstall that leaves a SYSTEM
  ; boot entry pointing at a deleted binary is worse than no uninstall at all. Silenced -- most
  ; machines never had either.
  nsExec::ExecToLog 'sc.exe stop "${LEGACY_SERVICE_NAME}"'
  Pop $0
  nsExec::ExecToLog 'schtasks /End /TN "${LEGACY_TASK_NAME}"'
  Pop $0
  nsExec::ExecToLog 'sc.exe delete "${LEGACY_SERVICE_NAME}"'
  Pop $0
  nsExec::ExecToLog 'schtasks /Delete /F /TN "${LEGACY_TASK_NAME}"'
  Pop $0
  ; `sc stop` only reaches an SCM-owned process; anything else keeps the binary locked and RMDir
  ; would silently leave it behind.
  ; The tray monitor rides along with the hub -- a leftover Run key pointing at a deleted file, or
  ; a running monitor polling a hub that no longer exists, are both worse than nothing.
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub-tray.exe'
  Pop $0
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${SERVICE_NAME}Tray"
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${LEGACY_SERVICE_NAME}Tray"
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub.exe /T'
  Pop $0
  Sleep 1500
  RMDir /r "$APPDATA\${DATA_DIR_NAME}\bin"
  Delete "$APPDATA\${DATA_DIR_NAME}\hub.json"
  Delete "$APPDATA\${DATA_DIR_NAME}\uninstall-hub.exe"
  RMDir "$APPDATA\${DATA_DIR_NAME}"
  ; ⚠️ THE PRE-2026-09-02 TREE ($APPDATA\${LEGACY_DATA_DIR_NAME}) IS DELIBERATELY LEFT ALONE.
  ; It is tempting to clear it here on the same argument used just above -- hub.json holds a cloud
  ; credential -- but during the rename cutover that file is the ONLY copy of a working hub's
  ; identity (hub id, member keys, the Shelly ingest secret, the LinkTap gateway/device ids), and
  ; carrying it across is a step in the migration runbook. An uninstaller that raced ahead of that
  ; step would destroy the thing the migration exists to preserve. Clearing the old tree is a
  ; separate, deliberate act once the migration is confirmed -- an owner decision, not a default.
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${SERVICE_NAME}"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_SERVICE_NAME}"
FunctionEnd

Section "Uninstall"
  nsExec::ExecToLog 'sc.exe stop "${SERVICE_NAME}"'
  Pop $0
  Sleep 1500
  nsExec::ExecToLog 'sc.exe delete "${SERVICE_NAME}"'
  Pop $0
  ; And both legacy persistence entries, for a machine upgraded from an older hub: the service under
  ; its former name, and the scheduled task from before the service existed.
  nsExec::ExecToLog 'sc.exe stop "${LEGACY_SERVICE_NAME}"'
  Pop $0
  nsExec::ExecToLog 'schtasks /End /TN "${LEGACY_TASK_NAME}"'
  Pop $0
  nsExec::ExecToLog 'sc.exe delete "${LEGACY_SERVICE_NAME}"'
  Pop $0
  nsExec::ExecToLog 'schtasks /Delete /F /TN "${LEGACY_TASK_NAME}"'
  Pop $0
  ; Same reason as RemoveEverything: a hub not owned by the SCM holds bin\brvg-hub.exe open,
  ; and RMDir would quietly skip it, leaving a working hub behind after a "successful" uninstall.
  ; The tray monitor rides along with the hub -- a leftover Run key pointing at a deleted file, or
  ; a running monitor polling a hub that no longer exists, are both worse than nothing.
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub-tray.exe'
  Pop $0
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${SERVICE_NAME}Tray"
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${LEGACY_SERVICE_NAME}Tray"
  nsExec::ExecToLog 'taskkill /F /IM brvg-hub.exe /T'
  Pop $0
  Sleep 1500
  RMDir /r "$INSTDIR\bin"
  Delete "$INSTDIR\hub.json"
  Delete "$INSTDIR\uninstall-hub.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${SERVICE_NAME}"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_SERVICE_NAME}"
SectionEnd

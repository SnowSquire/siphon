; Per-user installer for Siphon, built by makensis on Linux.
;
;   scripts/package-win.sh -DVERSION=... -DPAYLOAD=... -DOUTFILE=...
;
; The script is deliberately small: no registry beyond the two keys the updater
; and Apps & features need, no services, no machine-wide state. Everything
; lives under %LOCALAPPDATA%\Programs\Siphon, and the daemon keeps its data in
; %LOCALAPPDATA%\com.iken.siphon, which uninstall leaves alone.
;
; Silent install (`/S`) is the update hand-off: it closes both processes and
; lays the files down. The updater also passes `/RESTART`, and the section then
; reopens the window once the files are in place. A bare `/S` stays headless.

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !error "VERSION is not defined (pass -DVERSION=<x.y.z>)"
!endif
!ifndef PAYLOAD
  !error "PAYLOAD is not defined (pass -DPAYLOAD=<staged dir>)"
!endif
!ifndef OUTFILE
  !define OUTFILE "siphon-${VERSION}-x64-setup.exe"
!endif
!ifndef VCREDIST
  !error "VCREDIST is not defined (pass -DVCREDIST=<path to vcredist.ps1>)"
!endif

!define APP_NAME "Siphon"
!define APP_EXE "siphon.exe"
!define DAEMON_EXE "siphon-daemon.exe"
!define PUBLISHER "Ozeniken"
; update.rs reads InstallDir from here to tell an install from a portable copy.
!define REG_KEY "Software\Ozeniken\Siphon"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Siphon"

Name "${APP_NAME} ${VERSION}"
Caption "${APP_NAME} ${VERSION} Setup"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Siphon"
InstallDirRegKey HKCU "${REG_KEY}" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
SetCompressorDictSize 64
BrandingText "${APP_NAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "FileDescription" "${APP_NAME} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT OR Apache-2.0"

!define MUI_ICON "../crates/ui/icons/icon.ico"
!define MUI_UNICON "../crates/ui/icons/icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP_NAME}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; Both processes must be gone before their exes are replaced. A running exe
; cannot be overwritten on Windows; the installer is the one place that can
; close them without asking the app to cooperate.
!macro CloseSiphon
  nsExec::Exec '"$SYSDIR\taskkill.exe" /IM "${APP_EXE}" /F'
  Pop $0
  nsExec::Exec '"$SYSDIR\taskkill.exe" /IM "${DAEMON_EXE}" /F'
  Pop $0
  ; taskkill returns before the process has fully released its files.
  Sleep 400
!macroend

; The binaries use the dynamic CRT: the UCRT ships with Windows 10/11, and the
; VCRuntime/C++ runtime comes from the VC++ redistributable. When that is
; missing, install Microsoft's copy (one UAC prompt) instead of letting the app
; fail at launch with a missing MSVCP140.dll.
Function EnsureVCRuntime
  SetRegView 64
  ReadRegDWORD $0 HKLM "SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64" "Installed"
  SetRegView 32
  ${If} $0 == 1
    Return
  ${EndIf}

  DetailPrint "Microsoft Visual C++ runtime not found; installing it"
  InitPluginsDir
  File /oname=$PLUGINSDIR\vcredist.ps1 "${VCREDIST}"
  nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\vcredist.ps1"'
  Pop $0
  ${If} $0 == "0"
  ${OrIf} $0 == "3010"
    Return
  ${EndIf}

  DetailPrint "Visual C++ runtime install failed ($0)"
  IfSilent +2
  MessageBox MB_ICONEXCLAMATION|MB_OK "Siphon needs the Microsoft Visual C++ 2015-2022 Redistributable (x64). Install it from https://aka.ms/vs/17/release/vc_redist.x64.exe, then run this installer again."
FunctionEnd

Section "Siphon" SecMain
  SetShellVarContext current

  ; The updater's hand-off. GetOptions sets the error flag when the switch is
  ; absent, so latch the result before the section's file and registry work can
  ; disturb it. A plain `/S` install must not start the app by itself.
  ${GetParameters} $R0
  ${GetOptions} $R0 "/RESTART" $R1
  ${If} ${Errors}
    StrCpy $R1 0
  ${Else}
    StrCpy $R1 1
  ${EndIf}

  !insertmacro CloseSiphon
  Call EnsureVCRuntime

  ; A clean install directory: an upgrade that drops a DLL must not leave the
  ; old one behind, and nothing user-owned lives here.
  RMDir /r "$INSTDIR"
  SetOutPath "$INSTDIR"
  File /r "${PAYLOAD}/*"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "${REG_KEY}" "InstallDir" "$INSTDIR"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\Uninstall.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"

  ; An interactive install already offers this on the finish page; in silent
  ; mode that page never runs, so the update hand-off does the reopen here.
  ${If} $R1 == 1
    Exec '"$INSTDIR\${APP_EXE}"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  !insertmacro CloseSiphon

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\Uninstall.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"

  ; The install tree only. Config, logs and recordings stay in
  ; %LOCALAPPDATA%\com.iken.siphon for a reinstall to pick up.
  RMDir /r "$INSTDIR"

  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "${REG_KEY}"
SectionEnd

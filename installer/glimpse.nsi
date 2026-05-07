; ── Glimpse installer ────────────────────────────────────────────────────────
; Build with:  makensis /DVERSION="1.0.0" glimpse.nsi
; Or let the GitHub Actions workflow pass the version automatically.

!ifndef VERSION
  !define VERSION "0.0.0"
!endif

Unicode True
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "x64.nsh"

; ── Metadata ─────────────────────────────────────────────────────────────────
Name                "Glimpse"
OutFile             "glimpse-${VERSION}-setup.exe"
InstallDir          "$PROGRAMFILES64\Glimpse"
InstallDirRegKey    HKLM "Software\Glimpse" "InstallDir"
RequestExecutionLevel admin
BrandingText        "Glimpse ${VERSION}"

; Uncomment and point to a 256×256 .ico when you have one:
; !define MUI_ICON "..\assets\glimpse.ico"
; !define MUI_UNICON "..\assets\glimpse.ico"

; ── MUI pages ────────────────────────────────────────────────────────────────
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN          "$INSTDIR\glimpse.exe"
!define MUI_FINISHPAGE_RUN_TEXT     "Launch Glimpse now"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ── Install ───────────────────────────────────────────────────────────────────
Section "Glimpse" SecMain
  SetOutPath "$INSTDIR"

  ; Core binaries — built by CI and placed next to this script
  File "glimpse.exe"
  File "ffmpeg.exe"

  ; Uninstaller
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Registry: install location + Add/Remove Programs entry
  WriteRegStr HKLM "Software\Glimpse" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\Glimpse" "Version"    "${VERSION}"

  WriteRegStr   HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glimpse" \
                "DisplayName"     "Glimpse"
  WriteRegStr   HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glimpse" \
                "DisplayVersion"  "${VERSION}"
  WriteRegStr   HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glimpse" \
                "Publisher"       "glimpse"
  WriteRegStr   HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glimpse" \
                "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr   HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glimpse" \
                "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glimpse" \
                "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glimpse" \
                "NoRepair" 1

  ; Start Menu shortcut
  CreateDirectory "$SMPROGRAMS\Glimpse"
  CreateShortcut  "$SMPROGRAMS\Glimpse\Glimpse.lnk"           "$INSTDIR\glimpse.exe"
  CreateShortcut  "$SMPROGRAMS\Glimpse\Uninstall Glimpse.lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

; ── Uninstall ─────────────────────────────────────────────────────────────────
Section "Uninstall"
  ; Kill running instance first (best-effort)
  ExecWait 'taskkill /f /im glimpse.exe'

  Delete "$INSTDIR\glimpse.exe"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir  "$INSTDIR"

  Delete "$SMPROGRAMS\Glimpse\Glimpse.lnk"
  Delete "$SMPROGRAMS\Glimpse\Uninstall Glimpse.lnk"
  RMDir  "$SMPROGRAMS\Glimpse"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glimpse"
  DeleteRegKey HKLM "Software\Glimpse"
SectionEnd

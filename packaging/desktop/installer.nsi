Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "StrFunc.nsh"
!include "${INVENTORY_INCLUDE}"
${StrLoc}
Name "Paseo Sessions"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\Paseo Sessions"
InstallDirRegKey HKCU "Software\Paseo Sessions" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
Icon "${ICON}"
UninstallIcon "${ICON}"
VIProductVersion "${NUMERIC_VERSION}"
VIAddVersionKey /LANG=1033 "ProductName" "Paseo Sessions"
VIAddVersionKey /LANG=1033 "FileDescription" "Paseo Sessions Installer"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Paseo contributors"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; Refuse collisions with files not owned by an existing installation at this
; exact location. A shared user-selected directory is never implicitly owned.
Function PaseoCheckOwnedFile
  Pop $0
  ReadRegStr $3 HKCU "Software\Paseo Sessions" "InstallDir"
  StrCmp $3 $INSTDIR 0 unrelated
  ClearErrors
  FileOpen $1 "$INSTDIR\.paseo-sessions-owned-files" r
  IfErrors unrelated
  inventory_loop:
    ClearErrors
    FileRead $1 $2
    IfErrors inventory_missing
    StrCmp $2 "$0$\r$\n" owned
    StrCmp $2 "$0$\n" owned inventory_loop
  inventory_missing:
    FileClose $1
    Goto unrelated
  owned:
    FileClose $1
    Return
  unrelated:
    MessageBox MB_OK|MB_ICONSTOP "The selected folder contains a file not owned by Paseo Sessions:$\r$\n$0$\r$\nChoose another installation folder. No package files have been written."
    Abort
FunctionEnd

Function PaseoTrimNewline
  Exch $0
  trim_loop:
    StrCpy $5 $0 1 -1
    StrCmp $5 "$\r" trim
    StrCmp $5 "$\n" trim trimmed
  trim:
    StrCpy $0 $0 -1
    Goto trim_loop
  trimmed:
    Exch $0
FunctionEnd

; An upgrade replaces the previous payload at the same registered location,
; including a Flutter-era layout: delete every file its inventory owned and
; remove directories that became empty. Files the user created stay.
Function PaseoRemovePreviousFiles
  ReadRegStr $3 HKCU "Software\Paseo Sessions" "InstallDir"
  StrCmp $3 $INSTDIR 0 previous_done
  ClearErrors
  FileOpen $1 "$INSTDIR\.paseo-sessions-owned-files" r
  IfErrors previous_done
  previous_loop:
    ClearErrors
    FileRead $1 $2
    IfErrors previous_close
    Push $2
    Call PaseoTrimNewline
    Pop $2
    StrCmp $2 "" previous_loop
    StrCmp $2 ".paseo-sessions-owned-files" previous_loop
    ${StrLoc} $R0 $2 ".." ">"
    StrCmp $R0 "" 0 previous_loop
    ${StrLoc} $R0 $2 ":" ">"
    StrCmp $R0 "" 0 previous_loop
    Delete "$INSTDIR\$2"
    StrCpy $4 $2
    previous_parents:
      ${GetParent} $4 $4
      StrCmp $4 "" previous_loop
      RMDir "$INSTDIR\$4"
      Goto previous_parents
  previous_close:
    FileClose $1
  previous_done:
FunctionEnd

Section "Paseo Sessions" SEC_MAIN
  SetShellVarContext current
  !insertmacro PaseoCheckOwnedFiles
  Call PaseoRemovePreviousFiles
  SetOutPath "$INSTDIR"
  File /r "${SOURCE}\*"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  File /oname=.paseo-sessions-owned-files "${INVENTORY_MANIFEST}"
  WriteRegStr HKCU "Software\Paseo Sessions" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Classes\paseo" "" "URL:Paseo agent link"
  WriteRegStr HKCU "Software\Classes\paseo" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\paseo\DefaultIcon" "" '$"$INSTDIR\paseo_sessions.exe$",0'
  WriteRegStr HKCU "Software\Classes\AppUserModelId\sh.paseo.sessions.desktop" "DisplayName" "Paseo Sessions"
  WriteRegStr HKCU "Software\Classes\AppUserModelId\sh.paseo.sessions.desktop" "IconUri" "$INSTDIR\paseo_sessions.exe"
  WriteRegStr HKCU "Software\Classes\paseo\shell\open\command" "" '$"$INSTDIR\paseo_sessions.exe$" $"%1$"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\sh.paseo.sessions.desktop" "DisplayName" "Paseo Sessions"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\sh.paseo.sessions.desktop" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\sh.paseo.sessions.desktop" "UninstallString" '$"$INSTDIR\Uninstall.exe$"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\sh.paseo.sessions.desktop" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\sh.paseo.sessions.desktop" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\sh.paseo.sessions.desktop" "NoRepair" 1
  CreateShortCut "$SMPROGRAMS\Paseo Sessions.lnk" "$INSTDIR\paseo_sessions.exe"
  CreateShortCut "$DESKTOP\Paseo Sessions.lnk" "$INSTDIR\paseo_sessions.exe"
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  Delete "$SMPROGRAMS\Paseo Sessions.lnk"
  Delete "$DESKTOP\Paseo Sessions.lnk"
  ReadRegStr $0 HKCU "Software\Classes\paseo\shell\open\command" ""
  StrCmp $0 '$"$INSTDIR\paseo_sessions.exe$" $"%1$"' 0 +2
  DeleteRegKey HKCU "Software\Classes\paseo"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\sh.paseo.sessions.desktop"
  DeleteRegKey HKCU "Software\Paseo Sessions"
  ; Toast activation: the app registers this COM LocalServer32 and the AUMID's
  ; CustomActivator at startup; the AUMID key itself is written by this installer.
  DeleteRegKey HKCU "Software\Classes\CLSID\{72054DD0-FC41-4C29-8F0E-D407AFCC7265}"
  DeleteRegKey HKCU "Software\Classes\AppUserModelId\sh.paseo.sessions.desktop"
  !insertmacro PaseoRemoveOwnedFiles
  ; Daemon configuration, credentials, sessions and user CLI links are never deleted here.
SectionEnd

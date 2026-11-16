Unicode true
RequestExecutionLevel user
ManifestDPIAware true
SetCompressor /SOLID lzma

!include "MUI2.nsh"

!ifndef APP_EXE
  !define APP_EXE "target\release\psimole.exe"
!endif
!ifndef SETUP_EXE
  !define SETUP_EXE "PsiMole-Setup-x64.exe"
!endif
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\PsiMole"
!define MUI_ICON "assets\psimole.ico"
!define MUI_UNICON "assets\psimole.ico"
!define MUI_ABORTWARNING

Name "PsiMole"
OutFile "${SETUP_EXE}"
InstallDir "$LOCALAPPDATA\Programs\PsiMole"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Russian"

!macro StopApplication
  IfFileExists "$INSTDIR\PsiMole.exe" 0 done
  ExecWait '"$INSTDIR\PsiMole.exe" --quit-if-running' $0
  IntCmp $0 0 done
  Abort "Close PsiMole before continuing."
done:
!macroend

Section "PsiMole"
  !insertmacro StopApplication
  SetShellVarContext current
  SetOutPath "$INSTDIR"
  File /oname=PsiMole.exe "${APP_EXE}"
  File "..\LICENSE"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\PsiMole.lnk" "$INSTDIR\PsiMole.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "PsiMole"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\PsiMole.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "PsiMole"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "HelpLink" "https://github.com/lilter96/PsiMole"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  !insertmacro StopApplication
  SetShellVarContext current
  Delete "$SMPROGRAMS\PsiMole.lnk"
  Delete "$INSTDIR\PsiMole.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd

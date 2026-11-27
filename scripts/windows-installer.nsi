!include "MUI2.nsh"

!ifndef VERSION
!define VERSION "dev"
!endif
!ifndef SOURCE_DIR
!error "SOURCE_DIR is required"
!endif
!ifndef OUTPUT_FILE
!error "OUTPUT_FILE is required"
!endif

Name "MediaTalk"
OutFile "${OUTPUT_FILE}"
Unicode true
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\MediaTalk"
SetCompressor /SOLID lzma
BrandingText "MediaTalk ${VERSION}"

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

Section "Install"
    SetShellVarContext current
    SetOutPath "$INSTDIR"
    File /r "${SOURCE_DIR}\*"
    WriteUninstaller "$INSTDIR\Uninstall.exe"

    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "DisplayName" "MediaTalk"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "DisplayVersion" "${VERSION}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "Publisher" "MediaTalk"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "InstallLocation" "$INSTDIR"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "UninstallString" '"$INSTDIR\Uninstall.exe"'
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "NoModify" 1
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "NoRepair" 1

    CreateDirectory "$SMPROGRAMS\MediaTalk"
    CreateShortCut "$SMPROGRAMS\MediaTalk\MediaTalk.lnk" "$INSTDIR\run.bat" "gui" "$INSTDIR\media_talk_rust.exe" 0 SW_SHOWMINIMIZED
    CreateShortCut "$SMPROGRAMS\MediaTalk\Uninstall MediaTalk.lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
    SetShellVarContext current
    Delete "$SMPROGRAMS\MediaTalk\MediaTalk.lnk"
    Delete "$SMPROGRAMS\MediaTalk\Uninstall MediaTalk.lnk"
    RMDir "$SMPROGRAMS\MediaTalk"
    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk"
    RMDir /r "$INSTDIR"
SectionEnd

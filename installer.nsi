; ViewStage NSIS Installer Script
!include "MUI2.nsh"

Name "ViewStage"
OutFile "ViewStage-Setup.exe"
InstallDir "$PROGRAMFILES\ViewStage"
InstallDirRegKey HKLM "Software\ViewStage" "InstallDir"
RequestExecutionLevel admin

; 页面
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

Section "ViewStage"
    SetOutPath "$INSTDIR"

    ; 主程序
    File "target\release\viewstage.exe"

    ; 资源文件
    File /r "src\locales\*.*"
    File /r "src\assets\icons\*.*"

    ; 写入注册表
    WriteRegStr HKLM "Software\ViewStage" "InstallDir" "$INSTDIR"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ViewStage" "DisplayName" "ViewStage"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ViewStage" "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ViewStage" "InstallLocation" "$INSTDIR"

    ; 创建卸载程序
    WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
    Delete "$INSTDIR\viewstage.exe"
    Delete "$INSTDIR\uninstall.exe"
    RMDir /r "$INSTDIR\locales"
    RMDir /r "$INSTDIR\assets"
    RMDir "$INSTDIR"

    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ViewStage"
    DeleteRegKey HKLM "Software\ViewStage"
SectionEnd

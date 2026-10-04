@echo off
echo === ViewStage Release Build ===
echo.

echo [1/4] Building release...
cargo build --release
if %ERRORLEVEL% NEQ 0 (
    echo Build failed!
    exit /b 1
)

echo [2/4] Creating dist directory...
if not exist "dist" mkdir dist
if not exist "dist\locales" mkdir dist\locales
if not exist "dist\icons" mkdir dist\icons

echo [3/4] Copying files...
copy "target\release\viewstage.exe" "dist\"
xcopy /E /I /Y "src\locales\*" "dist\locales\"
xcopy /E /I /Y "src\assets\icons\*" "dist\icons\"

echo [4/4] Creating installer...
if exist "C:\Program Files (x86)\NSIS\makensis.exe" (
    "C:\Program Files (x86)\NSIS\makensis.exe" installer.nsi
    echo Installer created: ViewStage-Setup.exe
) else (
    echo NSIS not found, skipping installer creation.
    echo To create installer, install NSIS and run: makensis installer.nsi
)

echo.
echo Build complete!
echo   EXE: dist\viewstage.exe
echo.
pause

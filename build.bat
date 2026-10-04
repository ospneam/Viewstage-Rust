@echo off
echo === ViewStage Build Script ===
echo.

echo [1/3] Building release...
cargo build --release
if %ERRORLEVEL% NEQ 0 (
    echo Build failed!
    exit /b 1
)

echo [2/3] Creating output directory...
if not exist "dist" mkdir dist

echo [3/3] Copying files...
copy "target\release\viewstage.exe" "dist\"
copy "src\assets\icons\*" "dist\icons\" 2>nul
copy "src\locales\*" "dist\locales\" 2>nul

echo.
echo Build complete! Output: dist\
echo.
pause

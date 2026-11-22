@echo off
rem Builds plugin.exe (Windows backend) into the plugin folder with PyInstaller.
rem Requirements: Python 3.9+ for Windows (https://www.python.org/), "py" launcher in PATH.
setlocal
cd /d "%~dp0"
set PLUGIN_DIR=com.mrbretzel.keymacro.sdPlugin

py -3 -m pip install --upgrade pyinstaller || goto :error
py -3 -m PyInstaller --noconfirm --onefile --noconsole --name plugin ^
    --distpath "%PLUGIN_DIR%" --workpath build --specpath build ^
    --paths "%PLUGIN_DIR%" --hidden-import input_windows --exclude-module input_linux ^
    "%PLUGIN_DIR%\plugin.py" || goto :error

echo.
echo Built %PLUGIN_DIR%\plugin.exe
echo Run install.ps1 to copy the plugin into OpenDeck.
exit /b 0

:error
echo Build failed.
exit /b 1

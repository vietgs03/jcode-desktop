@echo off
rem Launch the debug build with Ctrl+R hot reload (slower UI).
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
set JCODE_DESKTOP_VERSION=0.6.6-wsl.1
cd /d C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop
start "" "C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop\target\debug\jcode-desktop.exe" %*

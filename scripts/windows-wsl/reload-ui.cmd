@echo off
rem Rebuild and hot-reload the UI of the running self-dev Desktop (same as Ctrl+R).
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
cd /d C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop
"C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop\target\debug\jcode-desktop.exe" --reload-ui

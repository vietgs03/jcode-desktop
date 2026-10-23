@echo off
rem Launch Jcode Desktop (optimized release build). Sessions default to WSL.
set JCODE_DESKTOP_VERSION=0.6.6-wsl.1
cd /d C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop
start "" "C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop\target\release\jcode-desktop.exe" %*

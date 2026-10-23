@echo off
rem Build Jcode Desktop (dev, hot-reload capable) natively on Windows with MSVC.
rem Usage: build.cmd [release]
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
set JCODE_DESKTOP_VERSION=0.6.6-wsl.1
cd /d C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop || exit /b 1
if "%1"=="release" (
  cargo build --release --bin jcode-desktop
) else (
  cargo build -p jcode-desktop -p jcode-desktop-ui
)
exit /b %ERRORLEVEL%

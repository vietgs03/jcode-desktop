@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
cd /d C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop || exit /b 1
set JCODE_WSL_TEST_REPO=/home/viethx/jcode-wt-demo
cargo test -p jcode-desktop-harness --lib session_git -- --nocapture
exit /b %ERRORLEVEL%

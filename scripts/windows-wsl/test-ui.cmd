@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
cd /d C:\Users\HASAKI\dev\jcode-desktop-wsl\jcode-desktop || exit /b 1
set JCODE_WSL_TEST_REPO=/home/viethx/jcode-wt-demo
start "" /wait /belownormal /b cargo test -p jcode-desktop-ui --lib wsl_changes_and_worktree_commands_end_to_end -- --nocapture --test-threads=1
exit /b %ERRORLEVEL%

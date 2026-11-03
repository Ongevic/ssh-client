@echo off
rem ============================================================
rem  Launch the ssh_grabber_rs TUI in its own terminal window.
rem  Optional arg %1 = starting local directory (Sigma passes
rem  the current folder here, so the TUI's LOCAL pane opens there).
rem ============================================================
setlocal
rem  Resolve the TUI binary relative to this script so the launcher works
rem  regardless of where the suite is checked out (%~dp0 ends with a backslash).
set "TUI=%~dp0ssh_grabber_rs\target\release\ssh_grabber_rs.exe"
set "STARTDIR=%~1"
if "%STARTDIR%"=="" set "STARTDIR=%USERPROFILE%"

if not exist "%TUI%" (
    echo ERROR: TUI binary not found at:
    echo   %TUI%
    echo Build it first:  cargo build --release   ^(run in ssh_grabber_rs^)
    pause
    endlocal
    exit /b 1
)

where wt.exe >nul 2>&1
if %errorlevel%==0 (
    wt.exe new-tab --title "SSH TUI" --startingDirectory "%STARTDIR%" "%TUI%"
) else (
    start "SSH TUI" /D "%STARTDIR%" "%TUI%"
)
endlocal

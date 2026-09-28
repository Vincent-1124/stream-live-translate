@echo off
setlocal
chcp 65001 >nul
if defined SLT_INSTALL_TEST_ROOT (
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-plugin.ps1" -DestinationRoot "%SLT_INSTALL_TEST_ROOT%" -SkipElevation -NoDialog
) else (
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-plugin.ps1"
)
set "install_exit=%ERRORLEVEL%"
echo.
if "%install_exit%"=="0" (
    echo 安装程序已成功完成。
) else (
    echo 安装未完成，请根据上方提示处理后重试。
)
echo 按任意键关闭此窗口。
pause
exit /b %install_exit%

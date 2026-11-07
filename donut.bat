@echo off
setlocal EnableExtensions

set "ROOT=%~dp0"
set "SYNC_DIR=%ROOT%donut-sync"
set "APP_TITLE=DONUT-DEV"
set "SYNC_TITLE=DONUT-SYNC"
set "APP_PID_FILE=%ROOT%.donut-dev.pid"
set "SYNC_PID_FILE=%ROOT%.donut-sync.pid"
set "PNPM=pnpm"

if "%~1"=="" goto start
if /i "%~1"=="start" goto start
if /i "%~1"=="stop" goto stop
if /i "%~1"=="restart" goto restart
if /i "%~1"=="status" goto status
if /i "%~1"=="sync" goto sync
if /i "%~1"=="sync-stop" goto sync_stop
if /i "%~1"=="help" goto help
goto help

:help
echo.
echo  Donut Browser 一键启停脚本
echo  ==========================
echo  用法: donut.bat [命令]
echo.
echo   start    启动 Donut 桌面开发版 (默认, 双击/无参数等同 start)
echo   stop     停止开发版, 同时停止正在运行的同步服务栈
echo   restart  重启开发版
echo   status   查看端口与进程状态
echo   sync     启动同步服务栈: MinIO + donut-sync, 需要 Docker
echo   sync-stop 停止同步服务栈
echo   help     显示本帮助
echo.
echo  首次运行会自动执行 pnpm install。
echo  启动后前端: http://localhost:12341  MCP: http://127.0.0.1:51080
echo.
exit /b 0

:use_pnpm
set "PNPM=pnpm"
pushd "%ROOT%"
pnpm --version >nul 2>nul
if not errorlevel 1 goto up_ok
where corepack >nul 2>nul
if errorlevel 1 goto up_fail
set "PNPM=corepack pnpm"
corepack pnpm --version >nul 2>nul
if not errorlevel 1 goto up_ok
:up_fail
popd
echo [错误] pnpm 不可用, 请先安装 Node.js 与 pnpm。
exit /b 1
:up_ok
popd
exit /b 0

:start
netstat -ano | findstr /C:":12341" | findstr /C:"LISTENING" >nul 2>nul
if not errorlevel 1 (
    echo [提示] 端口 12341 已被占用, Donut 开发版可能已在运行。
    echo        如需重启请先执行: donut.bat stop
    exit /b 0
)
call :use_pnpm
if errorlevel 1 exit /b 1
if exist "%ROOT%node_modules" goto deps_ok
echo [初始化] 首次运行, 正在安装依赖, 可能需要几分钟...
pushd "%ROOT%"
call %PNPM% install
if errorlevel 1 (
    popd
    echo [错误] pnpm install 失败, 请检查网络后重试。
    exit /b 1
)
popd
:deps_ok
echo [启动] 正在新窗口启动 Donut 开发版...
powershell -NoProfile -Command "Start-Process cmd -ArgumentList '/k','title %APP_TITLE% && %PNPM% tauri dev' -WorkingDirectory '%ROOT%' -PassThru | Select-Object -ExpandProperty Id | Set-Content -Encoding Ascii '%APP_PID_FILE%'"
echo.
echo   已打开 Donut 开发版窗口, 首次编译 Rust 需要几分钟, 请在该窗口查看进度。
echo   前端地址: http://localhost:12341
echo   MCP 服务: http://127.0.0.1:51080
echo   停止命令: donut.bat stop
echo.
exit /b 0

:stop
set "FOUND=0"
if not exist "%APP_PID_FILE%" goto stop_port
set /p DPID=<"%APP_PID_FILE%"
echo [停止] 结束开发版进程树 PID=%DPID% ...
taskkill /PID %DPID% /T /F >nul 2>nul && set "FOUND=1"
del /q "%APP_PID_FILE%" >nul 2>nul
:stop_port
netstat -ano | findstr /C:":12341" | findstr /C:"LISTENING" >nul 2>nul
if errorlevel 1 goto stop_devexe
set "FOUND=1"
for /f "tokens=5" %%P in ('netstat -ano ^| findstr /C:":12341" ^| findstr /C:"LISTENING"') do taskkill /PID %%P /T /F >nul 2>nul
:stop_devexe
powershell -NoProfile -Command "Get-Process donutbrowser -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '*\src-tauri\target\debug\*' } | Stop-Process -Force" >nul 2>nul
call :do_stop_sync
if "%FOUND%"=="1" echo [完成] Donut 开发版已停止。
if "%FOUND%"=="0" if "%SYNCFOUND%"=="0" echo [提示] 没有发现正在运行的 Donut 相关进程。
exit /b 0

:restart
call "%~f0" stop
goto start

:status
echo.
echo  Donut 开发环境状态
echo  ==================
echo.
echo  [进程]
tasklist /FI "IMAGENAME eq donutbrowser.exe" 2>nul | find /I "donutbrowser" >nul && echo   - donutbrowser.exe 开发版应用: 运行中 || echo   - donutbrowser.exe 开发版应用: 未运行
echo.
echo  [端口]
call :show_port 12341 "Next.js 前端"
call :show_port 12342 "donut-sync"
call :show_port 51080 "MCP 服务"
call :show_port 8987 "MinIO S3"
call :show_port 8988 "MinIO 控制台"
echo.
exit /b 0

:show_port
set "PORTSTATE=未运行"
for /f "tokens=5" %%P in ('netstat -ano ^| findstr /C:":%1" ^| findstr /C:"LISTENING"') do set "PORTSTATE=运行中 PID=%%P"
echo   - %~2 [%1]: %PORTSTATE%
exit /b 0

:sync
where docker >nul 2>nul || (echo [错误] 未找到 docker, 请先安装 Docker Desktop。 & exit /b 1)
docker info >nul 2>nul || (echo [错误] Docker 未运行, 请先启动 Docker Desktop。 & exit /b 1)
pushd "%SYNC_DIR%"
docker compose up -d
if errorlevel 1 (
    popd
    echo [错误] MinIO 容器启动失败, 请查看 docker 输出。
    exit /b 1
)
popd
echo [等待] MinIO 启动中...
set /a TRIES=0
:wait_minio
curl -sf http://127.0.0.1:8987/minio/health/live >nul 2>nul && goto minio_ready
set /a TRIES+=1
if %TRIES% geq 30 (
    echo [错误] MinIO 健康检查超时。
    exit /b 1
)
timeout /t 1 /nobreak >nul
goto wait_minio
:minio_ready
echo [完成] MinIO 就绪: S3 http://127.0.0.1:8987 , 控制台 http://127.0.0.1:8988 , 默认账号 minioadmin/minioadmin
if exist "%SYNC_DIR%node_modules" goto sync_deps_ok
call :use_pnpm
if errorlevel 1 exit /b 1
echo [初始化] 安装 donut-sync 依赖...
pushd "%SYNC_DIR%"
call %PNPM% install
if errorlevel 1 (
    popd
    echo [错误] pnpm install 失败, 请检查网络后重试。
    exit /b 1
)
popd
:sync_deps_ok
if exist "%SYNC_DIR%.env" goto sync_env_ok
echo [警告] 缺少 donut-sync\.env, 请先参照 donut-sync\.env.example 配置, 否则同步服务无法连接 MinIO。
:sync_env_ok
netstat -ano | findstr /C:":12342" | findstr /C:"LISTENING" >nul 2>nul
if not errorlevel 1 (
    echo [提示] donut-sync 已在运行: http://localhost:12342
    exit /b 0
)
call :use_pnpm
if errorlevel 1 exit /b 1
powershell -NoProfile -Command "Start-Process cmd -ArgumentList '/k','title %SYNC_TITLE% && %PNPM% start:dev' -WorkingDirectory '%SYNC_DIR%' -PassThru | Select-Object -ExpandProperty Id | Set-Content -Encoding Ascii '%SYNC_PID_FILE%'"
echo [完成] donut-sync 启动中: http://localhost:12342
echo [提示] 同步服务就绪后, 可执行 donut.bat 启动桌面应用。
exit /b 0

:sync_stop
call :do_stop_sync
exit /b 0

:do_stop_sync
set "SYNCFOUND=0"
if not exist "%SYNC_PID_FILE%" goto ss_port
set /p SPID=<"%SYNC_PID_FILE%"
taskkill /PID %SPID% /T /F >nul 2>nul && set "SYNCFOUND=1"
del /q "%SYNC_PID_FILE%" >nul 2>nul
:ss_port
netstat -ano | findstr /C:":12342" | findstr /C:"LISTENING" >nul 2>nul
if errorlevel 1 goto ss_docker
set "SYNCFOUND=1"
for /f "tokens=5" %%P in ('netstat -ano ^| findstr /C:":12342" ^| findstr /C:"LISTENING"') do taskkill /PID %%P /T /F >nul 2>nul
:ss_docker
netstat -ano | findstr /C:":8987" | findstr /C:"LISTENING" >nul 2>nul
if errorlevel 1 goto ss_report
set "SYNCFOUND=1"
where docker >nul 2>nul
if not errorlevel 1 (
    pushd "%SYNC_DIR%"
    docker compose down >nul 2>nul
    popd
)
:ss_report
if "%SYNCFOUND%"=="1" echo [完成] 同步服务栈已停止, MinIO 容器已下线。
exit /b 0

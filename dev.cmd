@echo off
rem KVBA 开发模式启动(vite + tauri)。
where npm >nul 2>nul
if %errorlevel%==0 (
  npm run tauri dev
) else (
  echo [KVBA] 未找到 npm,请安装 Node.js >=18 并确保其在 PATH 中
  exit /b 1
)

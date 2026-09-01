@echo off
rem KVBA 开发模式启动(vite + tauri)。本机 npm shim 失效时自动回退到 node 直调 npm-cli。
where npm >nul 2>nul
if %errorlevel%==0 (
  npm run tauri dev
) else (
  node "D:\nodejs\node_modules\npm\bin\npm-cli.js" run tauri dev
)

@echo off
set PATH=%PATH%;C:\Users\LocalUser\.cargo\bin
node -p "process.env.PATH.includes('.cargo')"
cd /d C:\Users\LocalUser\Documents\Codex\2026-09-16\openchamber\outputs\ClipLink
call npm run tauri dev
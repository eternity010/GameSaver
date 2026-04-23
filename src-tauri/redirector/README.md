# GameSaver Redirector (x64 MVP)

This folder contains the native artifacts used by the `launch_with_rule` real redirect flow:

- `gamesaver-injector.exe` (remote `LoadLibraryW` injector)
- `gamesaver-hook.dll` (IAT hook for `CreateFileW`)

## Build

Run in PowerShell (Developer Command Prompt with `cl.exe` available):

```powershell
cd src-tauri\redirector
.\build.ps1
```

Output files will be written to:

- `src-tauri\redirector\bin\gamesaver-injector.exe`
- `src-tauri\redirector\bin\gamesaver-hook.dll`

## Runtime contract

The Tauri backend writes a per-process config JSON to:

- `%TEMP%\gamesaver\redirect_config_<pid>.json`

The hook DLL reads that config on process attach and redirects paths that match `confirmedPaths` to `redirectRoot`.

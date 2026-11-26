; Paja Desk's Windows installer also installs `paja.exe` next to Desk and puts that folder on
; the user's PATH, so the CLI and the Claude Code plugin work without a separate install.
; Uninstalling takes the folder off PATH again. The folder reaches PowerShell through an
; environment variable, so no path needs quoting, and Path is edited in the registry as
; written (`%VAR%` entries stay unexpanded). Setting and clearing a dummy variable tells
; running programs that the environment changed. `$$` is a literal `$` for PowerShell.

!macro NSIS_HOOK_POSTINSTALL
  System::Call 'Kernel32::SetEnvironmentVariable(t "PAJA_DESK_DIR", t "$INSTDIR")i'
  nsExec::Exec `powershell.exe -NoProfile -NonInteractive -Command "$$d = $$env:PAJA_DESK_DIR; $$k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment'); $$p = [string]$$k.GetValue('Path', '', 'DoNotExpandEnvironmentNames'); $$parts = @($$p -split ';' | Where-Object { $$_ }); if ($$parts -notcontains $$d) { $$k.SetValue('Path', (@($$parts + $$d) -join ';'), 'ExpandString'); [Environment]::SetEnvironmentVariable('PAJA_DESK_REFRESH', '1', 'User'); [Environment]::SetEnvironmentVariable('PAJA_DESK_REFRESH', $$null, 'User') }"`
  Pop $0
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  System::Call 'Kernel32::SetEnvironmentVariable(t "PAJA_DESK_DIR", t "$INSTDIR")i'
  nsExec::Exec `powershell.exe -NoProfile -NonInteractive -Command "$$d = $$env:PAJA_DESK_DIR; $$k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment'); $$p = [string]$$k.GetValue('Path', '', 'DoNotExpandEnvironmentNames'); $$parts = @($$p -split ';' | Where-Object { $$_ }); $$kept = @($$parts | Where-Object { $$_ -ne $$d }); if ($$kept.Count -ne $$parts.Count) { $$k.SetValue('Path', ($$kept -join ';'), 'ExpandString'); [Environment]::SetEnvironmentVariable('PAJA_DESK_REFRESH', '1', 'User'); [Environment]::SetEnvironmentVariable('PAJA_DESK_REFRESH', $$null, 'User') }"`
  Pop $0
!macroend

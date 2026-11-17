; Installer hooks for the NSIS installer Tauri generates (bundle.windows.nsis.installerHooks).
;
; They set the virtual camera up after the files are copied and take it away before the uninstaller deletes the app.
; Both go through the elevated helper that is part of better-cam.exe (src-tauri/src/vcam_admin.rs); the installer is
; already elevated (per-machine), so there is no second prompt. Nothing here may fail the installation or show a box:
; if the camera is not set up, the app offers "Install virtual camera" in a banner on its first start.
;
; Helper exit codes: 0 done, 1 failed (see %ProgramData%\BetterCam\logs\install.log), 2 bad arguments,
; 5 not elevated, 10 done but takes effect after the camera service or Windows restarts, 11 another install is running.
;
; The macros use $0-$2 freely, like the rest of Tauri's script; nothing after the hooks reads them.

!macro NSIS_HOOK_POSTINSTALL
  ; The camera belongs to the account of the person at the desk. That is not always the account that runs this
  ; installer (an administrator may have entered their password over the user's shoulder), so the helper is told
  ; whose it is. Win32_ComputerSystem.UserName is the user of the console session; the expression prints their SID,
  ; or nothing for any failure (nobody signed in, a name that does not translate). The helper checks the SID again.
  ; `$$` is a literal `$` for NSIS. nsExec runs the program without a window; ExecToStack gives the exit code
  ; first, then what the program printed.
  nsExec::ExecToStack /TIMEOUT=60000 '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -Command "try { $$u = (Get-CimInstance Win32_ComputerSystem).UserName; if ($$u) { [Console]::Out.Write((New-Object System.Security.Principal.NTAccount($$u)).Translate([System.Security.Principal.SecurityIdentifier]).Value) } } catch { }"'
  Pop $0
  Pop $1
  ${If} $0 != "0"
    StrCpy $1 ""
  ${EndIf}
  StrCpy $2 $1 4
  ${If} $2 != "S-1-"
    DetailPrint "Virtual camera: not installed (could not tell which user is signed in). Better Cam offers to install it when it starts."
  ${Else}
    DetailPrint "Installing the virtual camera for $1 ..."
    ; No timeout: the helper must not be killed half way. It takes a machine-wide lock and ends by itself.
    nsExec::Exec '"$INSTDIR\${MAINBINARYNAME}.exe" --vcam-install --user-sid "$1"'
    Pop $0
    ${If} $0 == "0"
      DetailPrint "Virtual camera: installed."
    ${ElseIf} $0 == "10"
      DetailPrint "Virtual camera: installed. It takes effect after the camera service or Windows restarts."
    ${ElseIf} $0 == "11"
      DetailPrint "Virtual camera: not installed (another virtual camera install is running). Better Cam offers to install it when it starts."
    ${Else}
      DetailPrint "Virtual camera: not installed (exit code $0, see ProgramData\BetterCam\logs\install.log). Better Cam offers to install it when it starts."
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; The app holds the camera's frame file, so it has to be closed before the camera is removed. This is Tauri's own
  ; check (it runs again right after this hook and finds nothing). In silent mode it closes the app, or aborts the
  ; uninstall before anything is removed.
  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  ; /UPDATE is an update that installs again right after: the camera stays registered.
  ${If} $UpdateMode <> 1
  ${AndIf} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    DetailPrint "Removing the virtual camera ..."
    nsExec::Exec '"$INSTDIR\${MAINBINARYNAME}.exe" --vcam-uninstall'
    Pop $0
    ${If} $0 == "0"
      DetailPrint "Virtual camera: removed."
    ${ElseIf} $0 == "10"
      DetailPrint "Virtual camera: removed. The last files go after the camera service or Windows restarts."
    ${Else}
      DetailPrint "Virtual camera: not completely removed (exit code $0, see ProgramData\BetterCam\logs\install.log)."
    ${EndIf}
  ${EndIf}
!macroend

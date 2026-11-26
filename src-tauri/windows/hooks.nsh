; This file is included before the installer pages are declared.

; The finish page offers a desktop shortcut; the box starts unticked.
; (A silent or passive install has no finish page and creates the shortcut
; unless it is run with /NS.)
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED

; BoxIt keeps its settings and logs in %AppData%\BoxIt, not in the
; identifier-based folder that the stock uninstaller knows about.
; Remove it only when the user ticked "Delete the application data" and this
; is a real uninstall, not an update.
; The same registration the settings of the program can run, so a portable
; copy and an installed copy share one implementation.
!macro NSIS_HOOK_POSTINSTALL
  ExecWait '"$INSTDIR\BoxIt.exe" --shell register'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ExecWait '"$INSTDIR\BoxIt.exe" --shell unregister'
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    RMDir /r "$APPDATA\BoxIt"
  ${EndIf}
!macroend

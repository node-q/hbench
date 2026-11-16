; NSIS installer hooks (tauri.conf.json → bundle.windows.nsis.installerHooks).
;
; Up to 0.3.17 the app was named "Actuaria". The installer files an install under the product name (its folder,
; its Add/Remove Programs entry), so the renamed app installs beside the old one rather than over it, the in-app
; update included. Once the new one is in, run the old one's uninstaller silently, as Add/Remove Programs would.
; Its data is kept: it lives under the bundle identifier, which did not change and which the new app uses, and the
; uninstaller only deletes it when its "delete app data" box is ticked, which a silent run never does.

!macro NSIS_HOOK_POSTINSTALL
  Push $R0
  Push $R1
  ReadRegStr $R0 SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\Actuaria" "InstallLocation"
  ; Stored quoted: "C:\Users\me\AppData\Local\Actuaria"
  StrCpy $R1 $R0 1
  ${If} $R1 == '"'
    StrCpy $R0 $R0 -1 1
  ${EndIf}
  ${If} $R0 != ""
  ${AndIf} $R0 != $INSTDIR
  ${AndIf} ${FileExists} "$R0\uninstall.exe"
    DetailPrint "Removing the previous version, Actuaria, from $R0"
    ; `_?=` runs the uninstaller in place, so ExecWait waits for it; it then cannot delete itself or its folder.
    ExecWait '"$R0\uninstall.exe" /S _?=$R0'
    Delete "$R0\uninstall.exe"
    RMDir "$R0"
  ${EndIf}
  Pop $R1
  Pop $R0
!macroend

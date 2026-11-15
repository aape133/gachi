; Installer steps of our own, run by the installer script (app/installer.nsi) at the points
; Tauri's template offers.
;
; The DAW plugin is not part of the installer: it is a download of its own on the releases
; page, and the user puts it in the DAW's VST3 folder. So the installer needs no
; administrator rights, and neither installing nor uninstalling touches any VST3 folder.

!macro NSIS_HOOK_PREINSTALL
  ; a gachi installed for the whole computer (one build installed that way, into Program
  ; Files): take it out, so that two copies do not sit side by side. A silent uninstall
  ; keeps the settings.
  Push $0
  ReadRegStr $0 HKLM "${MANUPRODUCTKEY}" ""
  ${If} $0 != ""
  ${AndIf} $0 != $INSTDIR
  ${AndIf} ${FileExists} "$0\uninstall.exe"
    ExecWait '"$0\uninstall.exe" /S _?=$0'
    Delete "$0\uninstall.exe"
    RMDir "$0"
  ${EndIf}
  Pop $0
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; what earlier builds (which carried the plugin) left here: the plugin they carried along, and the note of
  ; which VST3 folder they copied it to. The copy in that folder stays (it is the user's
  ; now, to replace or delete; it usually needs administrator rights anyway).
  RMDir /r "$INSTDIR\vst3"
  DeleteRegValue SHCTX "${MANUPRODUCTKEY}" "Vst3Dir"
!macroend

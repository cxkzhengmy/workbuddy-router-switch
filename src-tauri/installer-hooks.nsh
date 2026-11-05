; 覆盖安装 / 卸载前先关掉占着文件的进程。
;
; 背景：内置采集服务 `agent-studio-runtime.exe` 是个**共享常驻进程**——
; 客户端退出后它继续活着（留给下一次连接复用），因此它 hold 着自己的 exe 句柄。
; Tauri 自带的 CheckIfAppIsRunning 只处理主程序 ${MAINBINARYNAME}.exe，
; 管不到这个 sidecar，结果是覆盖安装时报：
;     Error opening file for writing: ...\agent-studio-runtime.exe
; 用户只能中止、手动从托盘退出、再重试。
;
; 所以这里在提取文件之前把两个进程都结束掉。顺序是先主程序后 sidecar：
; 反过来的话主程序还活着，插件可能在两次 kill 之间把 sidecar 重新拉起来。

!macro NSIS_HOOK_PREINSTALL
  DetailPrint "关闭正在运行的应用…"
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /IM "${MAINBINARYNAME}.exe"'
  Pop $R9
  Sleep 400
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /IM "agent-studio-runtime.exe"'
  Pop $R9
  ; 文件句柄释放有延迟，覆盖前给系统一点时间，否则偶发仍然报占用
  Sleep 400
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "关闭正在运行的应用…"
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /IM "${MAINBINARYNAME}.exe"'
  Pop $R9
  Sleep 400
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /IM "agent-studio-runtime.exe"'
  Pop $R9
  Sleep 400
!macroend

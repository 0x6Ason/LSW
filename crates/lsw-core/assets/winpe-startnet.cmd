@echo off
setlocal EnableExtensions EnableDelayedExpansion
wpeinit
set "LSW_STATUS="
for %%D in (C D E F G H I J K L M N O P Q R S T U V W X Y Z) do (
    if exist "%%D:\lsw-status.tag" set "LSW_STATUS=%%D:\status.log"
)
for %%D in (C D E F G H I J K L M N O P Q R S T U V W X Y Z) do (
    if exist "%%D:\lsw\winpe-dism.cmd" (
        call "%%D:\lsw\winpe-dism.cmd"
        exit /b !errorlevel!
    )
    if exist "%%D:\lsw\apply-image.cmd" (
        call "%%D:\lsw\apply-image.cmd"
        exit /b !errorlevel!
    )
)
if defined LSW_STATUS >>"%LSW_STATUS%" echo LSW-WINPE-DISM failed launcher-missing
wpeutil.exe shutdown
exit /b 1

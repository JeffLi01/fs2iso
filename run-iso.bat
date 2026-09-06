set QEMU_OPTS=-machine q35 -m 512
set QEMU_OPTS=%QEMU_OPTS% -drive if=pflash,format=raw,unit=0,file="D:\develop\github.com\JeffLi01\fs2iso\tests\efi\assets\OVMF_CODE_4M.fd",readonly=on
set QEMU_OPTS=%QEMU_OPTS% -drive if=pflash,format=raw,unit=1,file="D:\develop\github.com\JeffLi01\fs2iso\tests\efi\assets\OVMF_VARS_4M_copy.fd"
set QEMU_OPTS=%QEMU_OPTS% -drive file=D:/develop/github.com/JeffLi01/test.iso,format=raw,media=cdrom

qemu-system-x86_64 %QEMU_OPTS%
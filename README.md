# fs2iso

纯 Rust 命令行工具：把你指定的文件/目录打包成一个 ISO 镜像，通过 BMC
Virtual Media 挂载后，可在 EFI Shell 中看到并读取/运行这些文件。

```text
fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...
```

## 用法（数据优先，引导可选）

```bash
fs2iso tools.iso D:\fw\efi_tools      # 数据盘：fs0 里看到 efi_tools/...
fs2iso --flat fix.iso FixPkg/         # 目录内容平铺到根
fs2iso -l BMC_TOOLS out.iso ./a ./b   # 多输入，各自按名入根
fs2iso out.iso .                      # 打包当前目录（. 内容入根）
```

默认产出的就是**普通数据盘**（ISO9660 + Joliet：base 树大写名兼容老读端，
Joliet 树保留原名/中文/长名），Windows 与带 ISO9660 驱动的固件 Shell 都可
直接浏览——**不需要、也不强求你提供引导文件**。

**引导是顺带能力**：只要 payload 里有一个名为 `EFI/BOOT/BOOTX64.EFI` 的
文件（想引导哪个 .efi，把它命名为这个名字放进去即可），fs2iso 就会自动把
全部 payload 镜像进一个 FAT 容器（`esp.img`）并加 El Torito 引导项——这类
镜像 EDK2 系固件也能引导，引导后 FAT 卷即 fs0，文件照常可见。
`--no-eltorito` 可压制引导（payload 有 BOOTX64.EFI 也不引导）。

选项：`-l/--label`、`--flat`、`--no-eltorito`、`-q/--quiet`。
退出码：0 成功；1 运行错误；2 CLI 用法错误。
防护：拒绝覆盖输入文件、同名大小写折叠冲突检测、junction/链接环检测。

## 构建与测试

```bash
cargo build --release          # target/release/fs2iso.exe
cargo test                     # 单元 + 5 集成：数据盘默认语义、Joliet 原名/内容逐字节、
                               # 有 BOOTX64.EFI 时的 FAT 容器读回全路径比对、错误路径等
py -3 scripts/verify_pycdlib.py out.iso payload --flat   # 开发期交叉验证（非交付物）
bash tests/efi/run_acceptance.sh   # QEMU+OVMF 真固件：引导盘单盘开机→fs0→startup.nsh
                                   # 读文件→mm 自退出码1；无 esp.img 的数据盘报 SKIP
```

验收资产（OVMF + EDK2 Shell）由 `tests/efi/fetch_assets.sh` 从 Debian 软件
包池获取（无需 github.com）。

## 设计要点与实测结论

- **payload 是数据，不是引导程序**：默认产物是可挂载、可浏览的数据盘；
  引导仅由 `EFI/BOOT/BOOTX64.EFI` 命名约定触发（你可自行改名任何 .efi），
  工具不内置 shell、不强求 boot。
- **为什么需要 esp.img**：EDK2 系固件（OVMF 等参考实现）没有 ISO9660 数据
  驱动，纯数据盘在其 shell 中不可见；FAT 卷是 UEFI 通用文件系统，引导后
  固件把 esp.img 挂为 fs0，文件全部可见（QEMU+OVMF 实测引导→读取 PASS）。
  传统固件（AMI 类）与 Windows 直接读 ISO9660 数据树即可，不受影响。
- UDF 未启用：hadris-cd 的 UDF 层不完整（缺 ECMA-167 文件集终止描述符），
  Windows udfs.sys 拒绝整卷（实测）；UDF bridge 变体见 git `9f17b72`。

## 架构

- CLI：clap 4.5（derive）；payload 收集/防护/摘要：本 crate（`src/lib.rs`）
- ISO9660+Joliet 写入：hadris-cd 2.3（纯 Rust）
- FAT 容器生成（引导盘时）：fatfs 0.3（纯 Rust；容量按 payload 动态，
  FAT16/32），`src/esp.rs`
- 交付物为单一 `fs2iso.exe`

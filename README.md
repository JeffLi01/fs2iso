# fs2iso

纯 Rust 命令行工具：把指定文件/目录打包成**可引导的 EFI Shell 光盘镜像**，
经 BMC Virtual Media 挂载（设为引导设备）后，UEFI 固件引导光盘 → EFI Shell
中直接看到并读取/运行这些文件。

```text
fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...
```

## 工作原理（为什么这样设计）

EDK2 系 UEFI 固件（OVMF 及多数 EDK2 血统固件）**不带 ISO9660 数据盘驱动**，
纯 ISO9660 数据光盘在 EFI Shell 里只有 `BLK`、没有 `fsX`——这是当年"挂载后
看不到文件"的根因（实测结论，非推测）。而 **FAT 是 UEFI 固件的通用文件系统**。
因此 fs2iso 的产物是双份 payload：

| 位置 | 读者 |
|---|---|
| ISO9660 + Joliet 数据树 | Windows 资源管理器 / AMI 类固件等传统读端 |
| **FAT 容器**（`esp.img`，El Torito 引导入口） | **EFI Shell（fs0）** —— 引导后固件把 FAT 卷挂为 fs0，payload 全部文件在此可见可运行 |

引导文件规则（面向 EFI shell 的严格默认）：自动采用 payload 里的
`EFI/BOOT/BOOTX64.EFI`，或 `--boot-efi <file>` 显式指定；payload 中
**没有引导文件时报错**（本工具产出的就是可引导镜像）。引导文件旁的同名
`startup.nsh` 会随 FAT 卷在引导后自动执行（EDK2 shell 特性）——把
`EFI/BOOT/startup.nsh` 放进 payload 即可让光盘插上即自动跑脚本。
`--no-eltorito` 保留纯数据盘输出（不引导、不建 FAT 容器）。

## 用法

```bash
# 打包成可引导 EFI Shell 镜像（payload 里需含 EFI/BOOT/BOOTX64.EFI 或给 --boot-efi）
fs2iso out.iso ./bmctools                # 目录保持（镜像根 = bmctools/...）
fs2iso --flat out.iso ./payload/         # 目录内容平铺到镜像根
fs2iso -l BMC_TOOLS_2024 out.iso ./bmctools
fs2iso --boot-efi EccProbe.efi out.iso ./dir   # 显式引导文件（须在 payload 内）
fs2iso --no-eltorito data.iso ./files    # 纯数据盘（Windows/AMI 用，不进 FAT）
```

选项：`-l/--label`、`--flat`、`--boot-efi <file>`、`--no-eltorito`、`-q/--quiet`。
退出码：0 成功；1 运行错误；2 CLI 用法错误。
防护：拒绝覆盖输入文件、同名大小写折叠冲突检测、junction/链接环检测。

## 构建与测试

```bash
cargo build --release          # target/release/fs2iso.exe
cargo test                     # 单元 + 6 集成（Joliet 原名/内容逐字节、El Torito 指向
                               # esp.img、FAT 容器 fatfs 读回全路径比对、严格模式报错等）
py -3 scripts/verify_pycdlib.py out.iso payload --flat   # 开发期交叉验证（非交付物）
bash tests/efi/run_acceptance.sh   # QEMU+OVMF 真固件：单盘引导→fs0→startup.nsh 读文件
                                   # →mm 写 isa-debug-exit 端口自退出(码1)；超时即 FAIL
```

验收环境资产（OVMF + EFI Shell 二进制）由 `tests/efi/fetch_assets.sh` 从
Debian 软件包池获取（无需 github.com）。

## 实测结论（QEMU 11 + OVMF/edk2 shell + Windows 11）

- **EFI Shell 可见性（EDK2）**：本工具默认产物单盘引导 OVMF 后，payload
  文件在 fs0 可见可读（qemu 自退出验收 PASS）。纯 ISO9660 数据盘在该固件下
  不可见（固件无 ISO9660 数据驱动），属固件限制。
- **Windows 挂载**：payload 在 ISO9660 数据树，资源管理器直接列出（原名经
  Joliet）；`esp.img`/`BOOT.CATALOG` 为工具工件。
- **格式演进史**：纯 ISO9660 → UDF bridge（EDK2 可读但 Windows udfs.sys
  拒绝 hadris-cd 的不完整 UDF，实测不可挂）→ **ISO9660+Joliet 数据树 +
  FAT ESP 容器**（当前）。UDF bridge 变体在 git `9f17b72`。

## 架构

- CLI：clap 4.5（derive）；payload 收集/防护/摘要：本 crate（`src/lib.rs`）
- ISO9660+Joliet 写入：hadris-cd 2.3（纯 Rust；UDF 关闭）
- FAT 容器生成：fatfs 0.3（纯 Rust；容量按 payload 动态，FAT16/32），
  `src/esp.rs`
- 无其它运行时依赖；交付物为单一 `fs2iso.exe`

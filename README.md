# fs2iso

纯 Rust 命令行工具：把指定文件/目录打包成一个**光盘镜像（UDF bridge）**，
经 BMC Virtual Media 挂载后，可在 UEFI 固件的 **EFI Shell**（fs0/fs1…）中
看到并读取这些文件/目录。

```text
fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...
```

## 为什么是 UDF bridge（三层文件系统）

真实固件实测（QEMU + OVMF + 官方 EDK2 Shell，见 `tests/efi/`）表明：
**EDK2 系 UEFI 固件的 Shell 不带 ISO9660 数据盘驱动** —— 纯 ISO9660 光盘
（无论由谁生成、结构多规范）挂载后只有 `BLKx`、没有 `fsX:`，文件不可见；
它只挂载 **UDF**。因此本工具输出 **UDF bridge** 镜像，同一份文件数据上叠
三个命名空间，覆盖三类读端：

| 命名空间 | 读端 |
|---|---|
| ISO9660（base，ASCII 大写） | 传统 BIOS/旧固件、通用 OS 光驱驱动 |
| Joliet（原文件名，含中文） | Windows Explorer 等 |
| UDF | **EDK2/EFI Shell**（BMC 场景的决定性读端） |

文件名在各命名空间规则内尽量保留原名（中文、空格、点开头、长名均可）。

## 用法

```bash
# 打包一个目录（目录名成为镜像根下的顶层目录）
fs2iso out.iso ./bmctools

# 目录内容直接摊到镜像根（mkisofs 风格）
fs2iso --flat out.iso ./payload/

# 指定卷标
fs2iso -l BMC_TOOLS_2024 out.iso ./bmctools

# 打包并启用 El Torito EFI 启动（自动识别 efi/boot/bootx64.efi）
fs2iso out.iso ./installer
```

选项：`-l/--label`、`--flat`、`--boot-efi <file>`（显式指定镜像内启动文件，
`--flat` 语义下可直接用磁盘路径）、`--no-eltorito`、`-q/--quiet`。
退出码：0 成功；1 运行错误；2 CLI 用法错误。

安全防护：拒绝覆盖输入文件（输出路径与 payload 冲突报错）、拒绝同名
大小写折叠冲突、检测 junction/链接目录环。

## 构建与测试

```bash
cargo build --release          # 产物 target/release/fs2iso.exe
cargo test                     # 单元 + 5 集成（独立 ISO9660 读回器校验
                               # Joliet 原名/内容逐字节、El Torito 指向、base 树）
py -3 scripts/verify_pycdlib.py out.iso payload --flat   # 开发期交叉验证（非交付物）
bash tests/efi/run_acceptance.sh   # QEMU+OVMF+EFI Shell 真固件验收（决定性门禁）
```

## 验收与已知限制（真实固件实测结论）

- **数据盘可见性已验收**：fs2iso 产物在 OVMF/EDK2 Shell 下挂载为 `fs1:`，
  根目录、子目录、中文名、文件内容均可读（`tests/efi/README.md` 记录矩阵：
  纯 ISO9660 在 EDK2 Shell 不可见，UDF bridge 可见）。
- **El Torito 直指 .efi 的引导项在 EDK2 固件上不可引导**（OVMF 报
  "failed to load … Not Found"）；EDK2 可引导光盘需 FAT-ESP 镜像形态。
  Data-CD 场景（先进 Shell 再挂载）不受影响。若需"插盘即引导"，后续按
  grub-mkrescue 风格改造 boot 段。
- Windows Explorer 挂载请用能读 UDF 的读端（Win10+ 原生可读 UDF bridge）；
  本机曾因幽灵虚拟光驱（盘符僵尸）导致任何 ISO 均 FS_NOT_READY，属宿主问题。

## 架构

- CLI：clap 4.5（derive）；payload 收集/防护/摘要：本 crate（`src/lib.rs`）
- 写入端：**hadris-cd 2.3**（纯 Rust；ISO9660+Joliet+UDF bridge 一体写入，
  数据层共享）
- 无其它运行时依赖；交付物为单一 `fs2iso.exe`

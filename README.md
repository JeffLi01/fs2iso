# fs2iso

纯 Rust 命令行工具：把指定文件/目录打包成一个**光盘镜像（UDF bridge）**，
经 BMC Virtual Media 挂载后，可在 UEFI 固件的 **EFI Shell**（fs0/fs1…）中
看到并读取这些文件/目录。

```text
fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...
```

## 输出格式：ISO9660 + Joliet（默认，实测定案）

`fs2iso` 输出 ISO9660 + Joliet 双层镜像（写入端 hadris-cd 2.3，纯 Rust，
数据层共享）。命名空间分工：

| 命名空间 | 用途 |
|---|---|
| ISO9660（base，ASCII 大写） | 传统固件 / 通用光驱驱动（Windows CDFS 挂载实测 ✅） |
| Joliet（原文件名，含中文） | Windows 资源管理器（优先显示 Joliet 原名） |

**格式演进结论（全部为实测，非推测）**：早期纯 ISO9660 布局经
QEMU+OVMF+EDK2 Shell 测试发现不可见 → 曾改用 hadris-cd 的 UDF bridge
（EDK2 Shell 可读，QEMU 验收过）；但**该 UDF 层不完整**（缺 ECMA-167
文件集终止描述符），EDK2 的 UdfDxe 宽容照读，**Windows udfs.sys 严格拒绝
整卷**（实测：bridge 镜像挂出盘符但不可读，纯 ISO9660 镜像同机秒挂）。
因此默认输出回到 ISO9660+Joliet——Windows 与主流（AMI 类）BMC 固件均可
读；Debian-OVMF 这类缺 ISO9660 数据驱动的 EDK2 固件是例外（其 shell 只
挂 UDF），需要 UDF bridge 变体时取 git 历史 `9f17b72`（等 UDF 写入端
规范补齐后再考虑回归）。

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

## 验收与已知限制（实测结论）

- **Windows 挂载已验收（决定性）**：`Mount-DiskImage` 对照实验——Nero/genisoimage
  参照 ISO、旧版纯 ISO9660 产物、当前默认产物（ISO9660+Joliet）**全部正常挂载并
  列出文件**；UDF bridge 变体被 Windows udfs.sys 拒绝（挂出盘符但卷不可读）。
- **EDK2-OVMF 特例**：该固件 shell 无 ISO9660 数据驱动（只挂 UDF），默认产物
  在其中有光驱设备但无 `fsX`——此为固件能力限制，非镜像缺陷（qemu 治具按
  镜像是否含 UDF 自适应断言，见 `tests/efi/`）。
- **El Torito 直指 .efi 的引导项在 EDK2 固件上不可引导**（OVMF 报
  "failed to load … Not Found"）；EDK2 可引导光盘需 FAT-ESP 镜像形态。
  Data-CD 场景（先进 Shell 再挂载）不受影响。若需"插盘即引导"，后续按
  grub-mkrescue 风格改造 boot 段。
- 若在实机遇到"挂载不了"，先做对照实验（同法挂一个已知良好 ISO）：参照盘
  也失败则是宿主/虚拟光驱栈问题（幽灵盘符或 ShellHWDetection 服务），非镜像。

## 架构

- CLI：clap 4.5（derive）；payload 收集/防护/摘要：本 crate（`src/lib.rs`）
- 写入端：**hadris-cd 2.3**（纯 Rust；ISO9660+Joliet 双层写入，数据层共享；
  UDF 层关闭，原因见"输出格式"节）
- 无其它运行时依赖；交付物为单一 `fs2iso.exe`

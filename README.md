# fs2iso

纯 Rust 编写的 **ISO9660 打包器**：把指定文件/目录打包成一个 ISO 镜像，通过 BMC
虚拟介质（Virtual Media）挂载为光驱后，可在 UEFI Shell 中直接访问其中的文件与目录
（`map -r` 后 `fs0:` 即可看到内容），也可让镜像本身从 EFI Shell 启动。

适用于：EFI Shell 下加载驱动/脚本/固件包、BIOS 更新、批量运维等场景，替代手工
`genisoimage`/`mkisofs`，无需在 Windows 上额外安装任何工具。

```text
fs2iso tools.iso D:\fw\efi_tools     # 打包目录
fs2iso --flat fix.iso FixPkg\        # 目录内容直接铺到镜像根
fs2iso --boot-efi Shell.efi shell.iso Shell.efi mydir\   # 指定 EFI 启动文件
```

挂载后（BMC → 虚拟介质 → 添加 CD/DVD 镜像）在服务器 UEFI Shell 中：

```text
Shell> map -r
Shell> fs0:
FS0:\> dir
```

## 特性

- **双命名空间镜像**：
  - ISO9660 基础树 —— 大写规范名（`A–Z 0–9 _`，≤30 字符，文件带 `;1` 版本号），
    任何 UEFI 固件都能读；
  - **Joliet 树** —— 保留原始文件名：大小写、空格、长名、中文等（Windows
    资源管理器与多数固件按此树显示，所见即源目录）。
- **El Torito EFI 启动项**（可选）：自动识别 payload 中的 `efi/boot/bootx64.efi`
  并生成 platform 0xEF 启动目录；也可用 `--boot-efi` 显式指定任意 `.efi`。
  带启动项的镜像可以直接引导到 EFI Shell（如 shell.efi），Shell 所在的光驱即 `fs0`。
- **体积标签**、`--flat`（mkisofs 式平铺）、`--no-joliet`、`--no-eltorito` 等选项。
- 库本体**零第三方依赖**（ISO9660 结构按 ECMA-119 手写）；CLI 用 `clap` 解析参数。

## 构建

```text
cargo build --release        # 产物 target/release/fs2iso.exe
cargo test                   # 单元测试 + 端到端读回校验（17 项）
```

需要 Rust 工具链（1.70+，edition 2021）。

## 用法

```text
fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...
```

每个 `PATH` 按自己的名字放进镜像根：文件 → 根下文件；目录 → 根下目录（整棵子树）。
`--flat` 时目录参数的内容并入镜像根（mkisofs 习惯）。

| 选项 | 说明 |
| --- | --- |
| `-l, --label <NAME>` | 卷标（默认取输出文件名） |
| `--flat` | 目录内容并入镜像根 |
| `--boot-efi <FILE>` | 把 payload 内该文件作为 El Torito EFI 启动镜像 |
| `--no-eltorito` | 完全不生成 El Torito 启动记录 |
| `--no-joliet` | 不生成 Joliet 命名空间 |
| `-q, --quiet` | 不输出打包摘要 |

示例：

```text
# 打包整个固件工具目录（保留目录结构）
fs2iso -l "FW_TOOLS" fwtools.iso D:\fw\efi_tools

# 把修复包内容直接铺在镜像根，便于 fs0:\ 一眼看到
fs2iso --flat -l "HOTFIX" hotfix.iso D:\pkg\hotfix\

# 用 shell.efi 直接引导 + 附带载荷（shell.iso 可引导，载荷在 fs0 可见）
fs2iso --boot-efi Shell.efi shell.iso Shell.efi D:\fw\efi_tools
```

### BMC 虚拟介质验证流程

1. 生成镜像后，在 BMC 管理页（iDRAC/iLO/浪潮 BMC 等）挂载为 **CD/DVD** 虚拟介质；
2. 服务器开/关机时以 UEFI 模式从该光驱引导（直接进 EFI Shell），
   或引导到已有 Shell 后执行 `map -r`；
3. `fs0:` 进入光驱，`dir` / `ls` 查看内容。

### 命名可见性说明

EFI Shell / Windows 中显示哪个命名空间取决于读取驱动的能力：

- 支持 Joliet 的驱动（Windows、多数固件）→ 显示**原始文件名**；
- 仅实现 ISO9660 的驱动 → 显示**大写规范名**（如 `README.TXT`，不带 `;1` 显示）。

两类名称都指向同一份文件内容，脚本请按实际显示名书写（建议 payload 本身就用
大写+`_` 命名以兼容两类驱动）。

## 镜像格式设计

扇区 2048 字节，卷布局：

```text
[0..16)          系统区（全零）
16               Primary Volume Descriptor
17               Boot Record VD（El Torito，可选）
18               Supplementary Volume Descriptor（Joliet，可选）
next             卷描述符集终止符
next             El Torito 启动目录（1 扇区，可选）
next             L/M 路径表（base、joliet 各一对）
next             base 命名空间目录块（DFS 序）
next             Joliet 命名空间目录块（DFS 序）
next             文件内容（逐扇区对齐）
```

要点：

- 两棵目录树各自拥有目录块与路径表，**文件内容区共用同一组 LBA 指针**；
- 目录记录 `.`=0x00、`..`=0x01（ECMA-119 9.1.4 结构字节，两命名空间一致）；
- 目录记录不跨扇区；空文件合法（长度 0，占位不占扇区）；
- El Torito 校验和、Boot Record VD 紧跟 PVD（第 17 扇区，mkisofs 惯例）。

## 已知限制

- 单文件 > 4 GiB 不支持（ISO9660 长度字段 32 位）；
- 目录层级建议 ≤ 8（ECMA-119 深度上限，未做自动降级/重定位）；
- 仅生成 ISO9660(+Joliet) 与 El Torito，不生成 UDF/Rock Ridge；
- 未内嵌任何 EFI Shell 二进制（`shell.efi` 需自备，注意 UEFI 授权）。

## 测试与验证

- `cargo test`：单元测试 + 端到端集成测试（内置独立最小 ISO9660 读回器，
  逐字段校验双树、路径表、扇区互斥、El Torito 校验和、文件内容一致）；
- 交叉验证（开发期，可选）：用第三方实现 pycdlib 读回镜像做全树遍历与内容比对：
  `scripts/verify_pycdlib.py out.iso <payload-dir>`（仅作验证工具，不进交付物）。

## 目录结构

```text
src/
  main.rs      CLI（clap）
  lib.rs       公共 API：build_iso / Options / BuildSummary
  layout.rs    卷布局与渲染（PVD/SVD/目录记录/路径表/El Torito）
  tree.rs      payload 收集（arena）
  names.rs     双命名空间命名规则
  timeutil.rs  UTC 日期换算
tests/
  integration.rs  端到端读回校验
scripts/
  verify_pycdlib.py  开发期 pycdlib 交叉验证（需 Python + pycdlib）
```

## Roadmap

- [ ] 真实 BMC + UEFI 固件环境验证清单化（fs0 可见性、引导、Joliet 显示）
- [ ] 可选 Rock Ridge 命名空间（Linux 友好）
- [ ] 目录层级超限时自动处理/明确报错

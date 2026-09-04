# fs2iso

纯 Rust 编写的 **ISO9660 打包器**：把指定文件/目录打包成一个 ISO 镜像，通过 BMC
虚拟介质（Virtual Media）挂载为光驱后，可在 UEFI Shell 中直接访问其中的文件与目录
（`map -r` 后 `fs0:` 即可看到内容），也可让镜像本身引导 EFI Shell。

适用于：EFI Shell 下加载驱动/脚本/固件包、BIOS 更新、批量运维等场景，替代手工
`genisoimage`/`mkisofs`，无需在 Windows 上额外安装任何工具。

```text
fs2iso tools.iso D:\fw\efi_tools     # 打包目录（目录按原名出现在镜像根）
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
  - ISO9660 基础树 —— 规范名（大写、`0-9 A-Z _`、文件带 `;1` 版本号；重名自动
    加 `_1/_2` 后缀；超长自动截断），任何 UEFI 固件都能读；
  - **Joliet 树** —— 保留原始文件名：大小写、空格、长名、中文等（Windows
    资源管理器与支持 Joliet 的固件按此显示，所见即源目录）。
- **El Torito EFI 启动**（可选）：自动识别 payload 中的 `efi/boot/bootx64.efi`，
  或 `--boot-efi` 显式指定任意 `.efi`，生成含 EFI 平台(0xEF)分节的启动目录。
  生成启动项时镜像根会出现 `boot.catalog` 文件（引擎自动创建，genisoimage 同款行为）。
- 体积标签、`--flat`（mkisofs 式平铺）、`--no-joliet`、`--no-eltorito` 等选项。
- ISO9660 结构与写入由 **[hadris-iso](https://crates.io/crates/hadris-iso)**（纯
  Rust，支持 Joliet/El Torito，作者维护 spec/ + fuzz/ + 交叉验证）负责；本 crate
  提供 CLI、payload 收集语义（keep-parent/flat、目录循环检测、重名报错）、启动文件
  解析与打包摘要。

## 构建

```text
cargo build --release        # 产物 target/release/fs2iso.exe
cargo test                   # 单元 + 集成测试（独立 ISO9660 读回器全量校验）
```

依赖 Rust 工具链（edition 2021）+ crates.io 拉取 `clap`、`hadris-iso`。

## 用法

```text
fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...
```

每个 `PATH` 按自己的名字放进镜像根：文件 → 根下文件；目录 → 根下目录（整棵子树）。
`--flat` 时目录参数的内容并入镜像根（mkisofs 习惯）。多个参数同名冲突会明确报错。

| 选项 | 说明 |
| --- | --- |
| `-l, --label <NAME>` | 卷标（默认取输出文件名） |
| `--flat` | 目录内容并入镜像根 |
| `--boot-efi <FILE>` | 把 payload 内该文件作为 El Torito 启动镜像 |
| `--no-eltorito` | 完全不生成 El Torito 启动记录 |
| `--no-joliet` | 不生成 Joliet 命名空间 |
| `-q, --quiet` | 不输出打包摘要 |

示例：

```text
# 打包整个固件工具目录（保留目录结构）
fs2iso -l "FW_TOOLS" fwtools.iso D:\fw\efi_tools

# 把修复包内容直接铺在镜像根，便于 fs0:\ 一眼看到
fs2iso --flat -l "HOTFIX" hotfix.iso D:\pkg\hotfix\

# 用 shell.efi 引导 + 附带载荷
fs2iso --boot-efi Shell.efi shell.iso Shell.efi D:\fw\efi_tools
```

### BMC 虚拟介质验证流程

1. 生成镜像后，在 BMC 管理页（iDRAC/iLO/浪潮 BMC 等）挂载为 **CD/DVD** 虚拟介质；
2. 服务器以 UEFI 模式从该光驱引导（直接进 EFI Shell），或引导到已有 Shell 后执行
   `map -r`；
3. `fs0:` 进入光驱，`dir` / `ls` 查看内容。

### 命名可见性说明

EFI Shell / Windows 中显示哪个命名空间取决于读取驱动的能力：

- 支持 Joliet 的驱动（Windows、多数固件）→ 显示**原始文件名**（含中文/空格/长名）；
- 仅实现 ISO9660 的驱动 → 显示**大写规范名**（如 `README.TXT`，冲突带 `_1`）。

两类名称指向同一份文件内容。脚本请按实际显示名书写（建议 payload 内文件本身用
大写+`_` 命名，两类驱动下都无需猜名）。

## 镜像结构

由 hadris-iso 生成，标准 ISO9660（+Joliet/El Torito）：

- PVD(16) → Boot Record(17，可选) → SVD(18，可选) → 终止符 → 路径表 → 目录块 →
  文件内容；扇区 2048 字节；
- 两棵目录树共享同一份文件数据；
- El Torito 启动目录含 x86 默认项 + **EFI 平台(0xEF)分节**（双平台形态，
  UEFI 固件按分节平台匹配引导）；
- 目录记录 `.`/`..` 为结构字节 0x00/0x01，Joliet 转义 `%/E`（level 3）。

## 已知限制

- 单文件 > 4 GiB 不支持（引擎拒绝）；
- 目录深度 ≤ 8、路径长度 ≤ 255（引擎校验并明确报错，可后续开 Rock Ridge 深度
  重定位）；
- 文件内容打包时整体读入内存（BMC 固件工具等 payload 通常数 MB，无碍；超大
  payload 注意内存占用）；
- 默认不生成 Rock Ridge/UDF；如需 Linux 长名/权限语义可开 Rock Ridge（后续选项）；
- 未内嵌 EFI Shell 二进制（`shell.efi` 需自备，注意 UEFI 授权）。

## 测试与验证

- `cargo test`：单元测试 + 8 项集成测试——用**独立最小 ISO9660 读回器**对引擎产物
  全量校验：Joliet/base 双树逐文件名称、大小与内容一致、El Torito 条目 RBA 指向
  启动文件 extent、`--no-joliet`/`--no-eltorito`/`--boot-efi`/平铺/重名/防覆盖等；
- 交叉验证（开发期，可选）：第三方实现 pycdlib 全树读取与内容比对：
  `scripts/verify_pycdlib.py out.iso <payload-dir>`（flat 镜像 + 源目录对比）。

## 目录结构

```text
src/
  lib.rs       收集 payload → hadris-iso InputTree；boot 解析；build_iso
  main.rs      CLI（clap）
tests/
  integration.rs  独立 ISO9660 读回器端到端校验
scripts/
  verify_pycdlib.py  开发期 pycdlib 交叉验证（需 Python + pycdlib）
```

## Roadmap

- [ ] 真实 BMC + UEFI 固件环境验证清单化（fs0 可见性、El Torito EFI 分节引导）
- [ ] 可选 Rock Ridge（Linux 长名/权限）、ISO9660:1999 命名
- [ ] 超大 payload 流式打包（当前整树读入内存）

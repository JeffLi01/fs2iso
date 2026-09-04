# fs2iso

纯 Rust 编写的 **ISO9660 打包器**：把指定文件/目录打包成一个 ISO 镜像，通过 BMC
虚拟介质（Virtual Media）挂载为光驱后，可在 UEFI Shell 中直接访问其中的文件与目录
（`map -r` 后 `fs0:` 即可看到内容），也可让镜像本身引导 EFI Shell。

适用于：EFI Shell 下加载驱动/脚本/固件包、BIOS 更新、批量运维等场景。

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

## 引擎与结构

ISO 写入使用 **[isobemak](https://crates.io/crates/isobemak)**（纯 Rust，
UEFI/BIOS El Torito 感知）。isobemak 0.4.x 存在两处规范缺陷，本工具在构建后执行
**一致性补强 pass**（`src/iso_fix.rs`）修复：

1. **路径表缺失**：isobemak 不写 ISO9660 路径表（PVD 指针为 0）。fs2iso 从最终目录
   记录重建 Type-L/Type-M 路径表追加到卷尾并回填 PVD；
2. **PVD 双端序字段**：卷集大小/序号/逻辑块大小/路径表大小仅写小端、大端副本为 0
   （严格读器会拒）。fs2iso 统一改写为 LE+BE 双份一致；
3. **El Torito validation platform**：isobemak 硬编码 0x00（x86）。启用 EFI 启动时
   改为 **0xEF** 并重算校验和，UEFI 固件才能认目录。

产物为**单一 ISO9660 命名空间**（无 Joliet/UDF）：ASCII 字符大写化，非 ASCII 字节
（如中文 UTF-8）与空格原样保留，文件带 `;1` 版本号；目录块/文件布局按引擎生成。

## 命名可见性

EFI Shell / Windows 中显示的是引擎生成的名称：

- 纯 ASCII 名 → 全大写（`readme.txt` → `README.TXT`；空格/点保留）；
- 含中文等非 ASCII → 原字节保留（中文名可直接辨认）；
- 长名不截断；重名会自动区分大小写写入，跨输入合并时的同名冲突会**明确报错**。

> 引擎单命名空间意味着无法同时保留"大写规范名 + 原样小写名"两套视图；EFI 驱动与
> Windows 读到的都是同一份名称。

## 构建

```text
cargo build --release        # 产物 target/release/fs2iso.exe
cargo test                   # 单元 + 集成测试（独立 ISO9660 读回器校验）
```

依赖 Rust 工具链（edition 2021）+ crates.io：`clap`、`isobemak`。

## 用法

```text
fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...
```

每个 `PATH` 按自己的名字放进镜像根：文件 → 根下文件；目录 → 根下目录（整棵子树）。
`--flat` 时目录参数的内容并入镜像根。注意：**空目录不会出现在镜像中**（引擎按文件
建树）；纯数据模式（不配启动文件）生成的仍是合法数据 CD。

| 选项 | 说明 |
| --- | --- |
| `-l, --label <NAME>` | 卷标（默认取输出文件名，ASCII 清洗≤32） |
| `--flat` | 目录内容并入镜像根 |
| `--boot-efi <FILE>` | 把 payload 内该文件作为 El Torito UEFI 启动镜像 |
| `--no-eltorito` | 不生成 El Torito 启动记录 |
| `-q, --quiet` | 不输出打包摘要 |

启动文件：不指定 `--boot-efi` 时自动识别 payload 中任意 `efi/boot/bootx64.efi`；
`--boot-efi` 指定的文件必须已包含在输入中。El Torito 目录为 platform **0xEF**
的 no-emulation 项，直接指向该文件（纯 ISO 形态，不生成 isohybrid/ESP FAT 包装）。

## 已知限制

- 单命名空间（无 Joliet/Rock Ridge/UDF）；原文件名中的小写不再单独保留
- 空目录不落盘；payload 文件构建时整体读入内存
- 单文件 > 4 GiB 与目录层级过深未专门处理（引擎限制）
- 未内嵌 EFI Shell 二进制（`shell.efi` 需自备，注意 UEFI 授权）

## 测试与验证

- `cargo test`：单元测试 + 5 项集成测试——独立最小 ISO9660 读回器对产物全量校验：
  每个 payload 文件名称/大小/内容一致、El Torito 条目 RBA 指向启动文件 extent、
  validation platform=0xEF、PVD 双端序字段与路径表真实存在、错误路径
  （启动文件不在 payload/重名/覆盖 payload 文件）、label/摘要；
- 交叉验证（开发期，可选）：pycdlib（严格第三方解析器）全树读取与内容比对：
  `scripts/verify_pycdlib.py out.iso <payload-dir>`。isobemak 原生产物会被
  pycdlib 拒绝（PVD 双端序不一致），补强后通过。

## 目录结构

```text
src/
  lib.rs       收集 payload → isobemak IsoImage；boot 解析；build_iso
  iso_fix.rs   一致性补强：追加路径表、PVD 双端序、El Torito platform=EFI
  main.rs      CLI（clap）
tests/
  integration.rs  独立 ISO9660 读回器端到端校验
scripts/
  verify_pycdlib.py  开发期 pycdlib 交叉验证（需 Python + pycdlib）
```

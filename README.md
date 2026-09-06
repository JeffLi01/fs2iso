# fs2iso

纯 Rust 命令行工具：把你指定的文件/目录打包成一个 ISO 镜像。通过 BMC
Virtual Media 挂载后，EFI Shell 中可以看到并读取/运行这些文件。

```text
fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...
```

## 原理

所有 payload 文件/目录按原路径、原名打包进一个 **FAT 镜像 `esp.img`**——
不管某个文件是不是 EFI 程序（不注入、不改名、不做任何特殊处理）。光盘的
El Torito 引导项指向 `esp.img`，固件因此会加载/暴露这个 FAT 卷；EFI Shell
里它就成了 fs0/fsX，全部文件可见。

同一份 payload 也保留在光盘的 **ISO9660(+Joliet) 数据树** 中（base 树大写
名、Joliet 树原名含中文/长名），供直接挂载数据树的读端使用（Windows
资源管理器、支持 ISO9660 的固件 shell）。

```text
产物 ISO
├── esp.img  (FAT 卷)  ← 全部文件/目录，原样
├── <payload...>       ← 同一份文件，ISO9660+Joliet 数据树
└── El Torito → esp.img

`esp.img` 与 `boot.catalog` 是工具内部工件，在数据树中以 ISO9660 隐藏属性
存在——Windows 资源管理器/普通列表看不到，只显示你打包的文件。
```

## 用法

```bash
fs2iso tools.iso D:\fw\efi_tools      # 文件/目录进 esp.img，同时进数据树
fs2iso --flat fix.iso FixPkg/         # 目录内容平铺到根
fs2iso -l BMC_TOOLS out.iso ./a ./b   # 多输入，各自按名入根
fs2iso out.iso .                      # 打包当前目录
fs2iso --no-eltorito data.iso files/  # 只要纯 ISO9660 数据盘（无 esp.img）
```

选项：`-l/--label`、`--flat`、`--no-eltorito`、`-q/--quiet`。
退出码：0 成功；1 运行错误；2 CLI 用法错误。
防护：拒绝覆盖输入文件、同名大小写折叠冲突检测、junction/链接环检测、
保留根级 `esp.img` 名（工具工件）防冲突。

## 构建与测试

```bash
cargo build --release          # target/release/fs2iso.exe
cargo test                     # 单元 + 5 集成：esp.img 无条件生成、fatfs 读回
                               # 逐路径比对(无注入文件)、Joliet 数据树原名/内容、
                               # --no-eltorito 纯数据盘、错误路径、label
py -3 scripts/verify_pycdlib.py out.iso payload --flat   # 开发期交叉验证（非交付物）
bash tests/efi/run_acceptance.sh   # QEMU+OVMF 真固件：从盘引导 → esp 卷(fs0) 里
                                   # payload 文件可读 → mm 自退出码 1
```

验收资产（OVMF 固件，内含 EFI Internal Shell，无需外部 shell）由
`tests/efi/fetch_assets.sh` 从 Debian 软件包池获取（无需 github.com）。

## 说明

- 设计要点：**文件就是文件**——工具不区分"引导文件"，不加默认 shell，不
  强制任何命名；想引导就在 payload 里放你自己的 EFI 程序（它会原样进入
  esp.img 并成为固件可启动的目标，路径随你）。
- 为什么需要 esp.img：部分固件（如 EDK2/OVMF 系参考实现）的 shell 不读
  ISO9660 数据盘，而 FAT 是 UEFI 通用文件系统；esp.img 让这类固件也能在
  shell 中看到文件（QEMU+OVMF 实测：引导后 fs0 列出并读取全部 payload）。
- `--no-eltorito` 产物 = 与 genisoimage 等价的标准数据盘（无 esp.img）。
- UDF 未启用：hadris-cd 的 UDF 层不完整（缺 ECMA-167 文件集终止描述符），
  历史 UDF-bridge 变体见 git `9f17b72`。

## 架构

- CLI：clap 4.5（derive）；payload 收集/防护/摘要：本 crate（`src/lib.rs`）
- ISO9660+Joliet 写入：hadris-cd 2.3（纯 Rust）
- FAT 容器生成：fatfs 0.3（纯 Rust；容量按 payload 动态，FAT16/32），
  `src/esp.rs`
- 交付物为单一 `fs2iso.exe`

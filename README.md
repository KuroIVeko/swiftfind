# SwiftFind

[English](#english) | [中文](#中文)

![Windows](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-blue) ![Rust](https://img.shields.io/badge/language-Rust-orange)

---

## English

SwiftFind is a fast file-name search tool for Windows, in the spirit of [Everything](https://www.voidtools.com/). It reads the NTFS Master File Table directly to index every file on your drives in seconds, then keeps the index current through the USN change journal. Results update as you type. The UI is built with [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui), the GPU-accelerated UI framework from the Zed editor.

### Features

- **Fast indexing**: reads the MFT of every NTFS volume directly. The first scan usually takes a few seconds, and later launches load from a compact on-disk cache.
- **Live updates**: follows the USN journal, so created, renamed and deleted files show up right away.
- **Instant search**: multi-threaded matching on every keystroke, sortable by name, path, size or date modified.
- **Search syntax**: wildcards, path matching, and filters for extension, size, date and type (see below).
- **Lives in the tray**: a global hotkey (default `Ctrl+Shift+E`) shows or hides the window, and closing the window only hides it.
- **Optional start at logon**: runs through a Task Scheduler task, so there's no UAC prompt at every logon.
- **Follows the system light/dark theme**.
- **Remembers your layout**: window position and size, plus column widths you can drag to adjust.
- **Portable**: a single statically linked exe with no runtime dependencies.

### Search syntax

Combine any of these with plain keywords, separated by spaces:

| Syntax | Meaning |
| --- | --- |
| `*.pdf`, `report??.docx` | Wildcards |
| `windows\ notepad` | A keyword containing `\` matches the full path |
| `ext:txt;md` | Extension is one of the list |
| `size:>10mb`, `size:1m..20m`, `size:large` | Size. Supports `<` `>` `<=` `>=` `=` and `a..b` ranges. Units are b/kb/mb/gb/tb, 1024-based. Keywords: `empty` `tiny` `small` `medium` `large` `huge` `gigantic` |
| `dm:today`, `dm:2024-05`, `dm:>=2024-05-01` | Date modified. Accepts `today` `yesterday` `thisweek` `lastweek` `thismonth` `lastmonth` `thisyear` `lastyear`, `YYYY[-MM[-DD]]`, comparisons and `..` ranges |
| `file:` / `folder:` | Files only / folders only |

### Keyboard shortcuts

| Key | Action |
| --- | --- |
| `Ctrl+Shift+E` | Show / hide the window (global, configurable) |
| `↑` `↓` `Ctrl+Home` `Ctrl+End` | Move the selection |
| `Enter` | Open |
| `Ctrl+Enter` | Open the containing folder |
| `Ctrl+Shift+C` | Copy the full path |
| `Ctrl+,` | Settings |

Right-click a result for more actions.

### Requirements

- 64-bit Windows 10 or 11
- NTFS volumes (other file systems are not indexed)
- Administrator rights, which reading the MFT requires. The app asks for UAC once at launch.

### Install

Download the zip from [Releases](../../releases), unzip it anywhere, and run `SwiftFind.exe`. The exe isn't code-signed, so if SmartScreen warns you, click **More info → Run anyway**.

The index cache and settings live in `%LOCALAPPDATA%\swiftfind`. To uninstall:

1. Turn off start at logon in Settings.
2. Quit from the tray menu.
3. Delete the exe and that folder.

### Build from source

Requirements: Rust (stable, MSVC toolchain) and the Visual Studio C++ build tools.

```powershell
git clone https://github.com/KuroIVeko/swiftfind.git
cd swiftfind
# Download the pinned gpui / wgpu / font-kit source snapshots into third_party\
powershell -ExecutionPolicy Bypass -File scripts\fetch_deps.ps1
cargo build --release
# Or build and pack dist\SwiftFind-<version>-x64.zip
powershell -ExecutionPolicy Bypass -File scripts\package.ps1
```

### How it works

- `src/mft.rs`: full scan by reading the raw `$MFT` off the volume in large sequential chunks, parsed by worker threads.
- `src/ntfs.rs`: volume access, the `FSCTL_ENUM_USN_DATA` fallback scan, and USN journal reads.
- `src/indexer.rs`: a background thread that loads the cache, replays the journal from the cached position (or rescans), then applies changes in batches.
- `src/index.rs`, `src/index/`: an in-memory index with compact 20-byte entries, plus query parsing, filters and parallel search.
- `src/persist.rs`, `src/index/cache.rs`: the on-disk cache, so later launches start instantly.
- `src/app.rs`: the gpui UI. `src/tray.rs`, `src/hotkey.rs` and `src/autostart.rs` handle the tray, the global hotkey and start at logon.

---

## 中文

SwiftFind 是一款 Windows 上的极速文件名搜索工具，灵感来自 [Everything](https://www.voidtools.com/)。它直接读取 NTFS 主文件表（MFT），几秒内就能索引磁盘上的全部文件，之后通过 USN 变更日志实时保持索引最新，输入即出结果。界面基于 Zed 编辑器的 GPU 加速 UI 框架 [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui)。

### 功能

- **极速索引**：直接读取每个 NTFS 卷的 MFT。首次扫描通常只需几秒，之后从紧凑的磁盘缓存秒开。
- **实时更新**：监听 USN 日志，新建、重命名、删除的文件会立即反映出来。
- **即时搜索**：每次按键都会多线程匹配，可按名称、路径、大小、修改时间排序。
- **搜索语法**：支持通配符、路径匹配，以及扩展名、大小、日期、类型过滤（见下文）。
- **常驻托盘**：用全局热键（默认 `Ctrl+Shift+E`）显示或隐藏窗口，关闭窗口只是隐藏。
- **开机自启（可选）**：通过计划任务启动，每次登录都不会弹出 UAC。
- **自动跟随系统深色 / 浅色模式**。
- **记住布局**：记住窗口位置和大小，列宽也可以拖动调整。
- **绿色便携**：单个静态链接的 exe，不依赖任何运行库。

### 搜索语法

以下语法可以与普通关键词任意组合，用空格分隔：

| 语法 | 含义 |
| --- | --- |
| `*.pdf`、`报告??.docx` | 通配符 |
| `windows\ notepad` | 含 `\` 的关键词匹配完整路径 |
| `ext:txt;md` | 扩展名为列表中任意一个 |
| `size:>10mb`、`size:1m..20m`、`size:large` | 大小。支持 `<` `>` `<=` `>=` `=` 和 `a..b` 范围，单位 b/kb/mb/gb/tb（按 1024 进制）。关键字：`empty` `tiny` `small` `medium` `large` `huge` `gigantic` |
| `dm:today`、`dm:2024-05`、`dm:>=2024-05-01` | 修改时间。支持 `today` `yesterday` `thisweek` `lastweek` `thismonth` `lastmonth` `thisyear` `lastyear`、`YYYY[-MM[-DD]]`、比较符和 `..` 范围 |
| `file:` / `folder:` | 只看文件 / 只看文件夹 |

### 快捷键

| 按键 | 操作 |
| --- | --- |
| `Ctrl+Shift+E` | 显示 / 隐藏窗口（全局，可修改） |
| `↑` `↓` `Ctrl+Home` `Ctrl+End` | 移动选中项 |
| `Enter` | 打开 |
| `Ctrl+Enter` | 打开所在文件夹 |
| `Ctrl+Shift+C` | 复制完整路径 |
| `Ctrl+,` | 设置 |

右键结果可以看到更多操作。

### 系统要求

- 64 位 Windows 10 或 11
- NTFS 格式的磁盘（其他文件系统不会被索引）
- 管理员权限（读取 MFT 需要），启动时会弹出一次 UAC 确认

### 安装

从 [Releases](../../releases) 下载 zip，解压到任意位置，运行 `SwiftFind.exe`。程序没有数字签名，如果出现 SmartScreen 提示，点「更多信息」→「仍要运行」即可。

索引缓存和设置保存在 `%LOCALAPPDATA%\swiftfind`。卸载步骤：

1. 在设置中关闭开机自启。
2. 从托盘菜单退出。
3. 删除 exe 和上述文件夹。

### 从源码构建

需要 Rust（stable，MSVC 工具链）和 Visual Studio C++ 生成工具。

```powershell
git clone https://github.com/KuroIVeko/swiftfind.git
cd swiftfind
# 把锁定版本的 gpui / wgpu / font-kit 源码快照下载到 third_party\
powershell -ExecutionPolicy Bypass -File scripts\fetch_deps.ps1
cargo build --release
# 或者构建并打包成 dist\SwiftFind-<版本>-x64.zip
powershell -ExecutionPolicy Bypass -File scripts\package.ps1
```

### 实现概要

- `src/mft.rs`：直接从卷上大块顺序读取原始 `$MFT`，由工作线程并行解析。
- `src/ntfs.rs`：卷访问、`FSCTL_ENUM_USN_DATA` 备用扫描，以及 USN 日志读取。
- `src/indexer.rs`：后台线程。先加载缓存，再从缓存位置回放日志（不行就重新扫描），之后批量应用变更。
- `src/index.rs`、`src/index/`：内存索引（每条 20 字节），以及查询解析、过滤器和并行搜索。
- `src/persist.rs`、`src/index/cache.rs`：磁盘缓存，之后启动秒开。
- `src/app.rs`：gpui 界面。`src/tray.rs`、`src/hotkey.rs`、`src/autostart.rs` 分别负责托盘、全局热键和开机自启。

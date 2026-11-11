SwiftFind {VERSION} — 快速文件名搜索

【运行】
解压到任意文件夹，双击 SwiftFind.exe。程序需要管理员权限读取 NTFS 主文件表，
启动时会弹出一次 UAC 确认。首次运行会建立索引（通常几秒），之后从缓存秒开。

【系统要求】
64 位 Windows 10 / 11；只索引 NTFS 格式的磁盘。

【使用】
- 全局热键 Ctrl+Shift+E 显示 / 隐藏窗口（可在设置中修改）
- 关闭窗口只是隐藏到托盘；右键托盘图标 →「退出」才会真正退出
- Enter 打开，Ctrl+Enter 打开所在文件夹，Ctrl+Shift+C 复制路径，右键有更多操作
- 点击列标题排序，拖动列标题边缘调整列宽；Ctrl+, 打开设置（热键、开机自动启动）
- 窗口位置、大小和列宽会自动记住

【搜索语法】（可与普通关键词任意组合，空格分隔）
- 通配符：*.pdf   报告??.docx
- 路径：  含 \ 的关键词匹配完整路径，如 windows\ notepad
- ext:txt;md        扩展名
- size:>10mb        大小（也可 <、>=、<=、1m..20m、empty/tiny/small/medium/large/huge/gigantic）
- dm:today          修改时间（也可 yesterday、thisweek、lastweek、thismonth、lastmonth、
                    thisyear、lastyear、2024、2024-05、>=2024-05-01、2024-01..2024-03）
- file: / folder:   只看文件 / 只看文件夹

【首次运行提示"Windows 已保护你的电脑"】
程序没有数字签名，点「更多信息」→「仍要运行」即可。

【数据位置】
索引缓存和设置保存在 %LOCALAPPDATA%\swiftfind，不会写入程序所在文件夹。

【卸载】
1. 如果开启过开机自动启动，先在设置中关闭
2. 右键托盘图标 →「退出」
3. 删除 SwiftFind.exe 和 %LOCALAPPDATA%\swiftfind 文件夹

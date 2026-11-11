SwiftFind {VERSION} — fast file-name search

[Running]
Unzip anywhere and double-click SwiftFind.exe. It needs administrator rights to
read the NTFS Master File Table, so it asks for UAC once at launch. The first run
builds the index (usually a few seconds); later launches load it from the cache.

[Requirements]
64-bit Windows 10 / 11. Only NTFS drives are indexed.

[Usage]
- Global hotkey Ctrl+Shift+E shows / hides the window (configurable in Settings)
- Closing the window only hides it to the tray; right-click the tray icon -> Quit to exit
- Enter opens, Ctrl+Enter opens the containing folder, Ctrl+Shift+C copies the path;
  right-click for more
- Click a column header to sort, drag a header edge to resize; Ctrl+, opens Settings
  (hotkey, start at logon)
- Window position, size and column widths are remembered
- The language follows Windows (Chinese or English). To force one, add the line
  language=en  (or language=zh)  to %LOCALAPPDATA%\swiftfind\settings.ini

[Search syntax] (combine freely with plain keywords, separated by spaces)
- Wildcards: *.pdf   report??.docx
- Paths:     a keyword containing \ matches the full path, e.g. windows\ notepad
- ext:txt;md        extension
- size:>10mb        size (also <, >=, <=, 1m..20m, empty/tiny/small/medium/large/huge/gigantic)
- dm:today          date modified (also yesterday, thisweek, lastweek, thismonth, lastmonth,
                    thisyear, lastyear, 2024, 2024-05, >=2024-05-01, 2024-01..2024-03)
- file: / folder:   files only / folders only

["Windows protected your PC" on first run]
The program isn't code-signed. Click "More info" -> "Run anyway".

[Data]
The index cache and settings are stored in %LOCALAPPDATA%\swiftfind, never next to the exe.

[Uninstall]
1. If you turned on start at logon, turn it off in Settings
2. Right-click the tray icon -> Quit
3. Delete SwiftFind.exe and the %LOCALAPPDATA%\swiftfind folder

# 内置 ConPTY

微软官方 NuGet 包 `Microsoft.Windows.Console.ConPTY` 1.25.260930003（MIT 协议）里的 `conpty.dll` 与 `OpenConsole.exe`，
按架构分目录（`x64/`、`arm64/`），由安装包装到可执行文件同目录。

为什么要带：系统自带的旧版 ConPTY 在键盘（alt/esc）、鼠标模式转发和吞吐上都不如新版，TUI 里会出现滚轮、alt+键失效和输入卡顿。
这两个文件放在 exe 同目录，alacritty 的 `LoadLibraryW("conpty.dll")` 按默认搜索顺序先找应用目录，就会优先加载它们，不受 PATH 里别家软件影响。

本地 `cargo run` 想用新版：把对应架构目录下的两个文件复制到 `target/<profile>/`。

升级：从 nuget.org 下载新版 nupkg（zip），`runtimes/win-<arch>/native/conpty.dll` 与
`build/native/runtimes/<arch>/OpenConsole.exe` 覆盖进来，并更新上面的版本号。

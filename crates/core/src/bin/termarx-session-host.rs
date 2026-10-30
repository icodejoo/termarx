fn main() -> anyhow::Result<()> {
    // host 会创建 PTY，同样要避免从 PATH 捡到别家的 conpty.dll
    termy_core::restrict_dll_search_path();
    let mut args = std::env::args_os().skip(1);
    let first = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("session host directory required"))?;
    let root = if first == "--multiplexer-host" {
        args.next()
            .ok_or_else(|| anyhow::anyhow!("session host directory required"))?
    } else {
        first
    };
    termy_core::multiplexer::serve(std::path::Path::new(&root))
}

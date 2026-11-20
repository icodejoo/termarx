/// 系统提交量耗尽时先退避重试分配，避免默认的 abort 连带丢掉 host 里所有终端任务。
#[global_allocator]
static ALLOCATOR: termy_core::retry_alloc::RetryingAlloc<std::alloc::System> =
    termy_core::retry_alloc::RetryingAlloc::new(std::alloc::System);

fn main() -> anyhow::Result<()> {
    ALLOCATOR.set_patient_mode();
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

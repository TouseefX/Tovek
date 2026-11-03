//! Standalone `web-server` binary: the mimalloc allocator, the tokio runtime
//! with decompile-sized stacks, logging, and a listener on
//! [`web_server::BIND_ADDR`] — all routing/limit logic lives in the library
//! (`src/lib.rs`) so the GUI embeds the exact same server.

use std::io;

use tokio::net::TcpListener;
use tracing::info;

// Global allocator for the server binary. The decompiler is allocation-bound, so
// mimalloc's per-thread free-lists noticeably cut wall time (see Cargo.toml). It
// lives here in the binary, never in the shared library (which the wasm worker
// links and cannot build mimalloc against).
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<(), io::Error> {
    // Every decompile runs on a blocking thread and rayon's workers: both get
    // the decompile stack, or a long `elseif` chain overflows it and aborts
    // the server (a 900-arm chain did with the default 2 MiB).
    let _ = luau_lifter::configure_thread_pool(web_server::decompile_threads()?);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(luau_lifter::DECOMPILE_STACK_BYTES)
        .build()?
        .block_on(serve())
}

async fn serve() -> Result<(), io::Error> {
    // One process-global quiet panic hook, installed before any decompile work, so
    // the per-function/per-item `catch_unwind`s used by the batch path don't spam
    // stderr and don't race a per-call set_hook across threads.
    luau_lifter::install_quiet_panic_hook();

    // Setup the logger
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("failed to set global tracing subscriber");

    let app = web_server::default_router()?;

    // Run the web server
    let listener = TcpListener::bind(web_server::BIND_ADDR).await?;
    info!("🚀 Listening on {}", listener.local_addr()?);
    info!("Decompiling on {} threads ({} to change).",
        rayon::current_num_threads(), web_server::THREADS_ENV);
    axum::serve(listener, app).await
}

//! `zerosyntax-lsp`: the IDE-agnostic language server for C&C Generals: Zero Hour
//! INI files. Speaks LSP over stdio.

mod backend;
mod cache;
mod cli;
mod convert;
mod progress;
mod scan;
mod uri;

use backend::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() > 1 && !(args.len() == 2 && args[1] == "--stdio") {
        return cli::run(args);
    }

    // Log to stderr (stdout is reserved for the LSP wire protocol).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::build(Backend::new)
        .custom_method("zerosyntax/readVirtualFile", Backend::read_virtual_file)
        .custom_method("zerosyntax/indexCachePath", Backend::index_cache_path)
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;
    std::process::ExitCode::SUCCESS
}

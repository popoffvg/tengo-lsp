mod backend;
mod completion;
mod definition;
mod document;
mod exports;
mod format;
mod hover;
mod outline;
mod references;
mod rename;
mod resolver;
mod scope;
mod symbols;

use backend::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    // CLI subcommand: `tengo-lsp fmt [--write] [files...]`. No subcommand runs
    // the LSP server over stdio (the default).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("fmt") {
        std::process::exit(format::run_cli(&args[2..]));
    }

    env_logger::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

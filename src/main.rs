use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use clap::Parser;
use miden_lsp::backend::Backend;
use tower::Service;
use tower_lsp::{LspService, Server, jsonrpc::Request};

struct ExitAwareService {
    inner: LspService<Backend>,
    shutdown: Arc<AtomicBool>,
    exit: tokio::sync::watch::Sender<Option<i32>>,
}

impl Service<Request> for ExitAwareService {
    type Error = <LspService<Backend> as Service<Request>>::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    type Response = <LspService<Backend> as Service<Request>>::Response;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let is_exit = request.method() == "exit";
        let is_shutdown = request.method() == "shutdown";
        let future = self.inner.call(request);
        let shutdown = self.shutdown.clone();
        let exit = self.exit.clone();
        Box::pin(async move {
            let response = future.await;
            if is_shutdown
                && response.as_ref().is_ok_and(|response| {
                    response.as_ref().is_some_and(|response| response.error().is_none())
                })
            {
                shutdown.store(true, Ordering::Relaxed);
            }
            if is_exit {
                let _ = exit.send(Some(if shutdown.load(Ordering::Relaxed) {
                    0
                } else {
                    1
                }));
            }
            response
        })
    }
}

fn init_tracing() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "miden_lsp=info".into()),
        )
        .with_writer(std::io::stderr)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

fn main() {
    #[derive(Parser)]
    #[command(version, about = "Miden Assembly language server")]
    struct Options {
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        toolchain: Option<String>,
        #[arg(long, default_value = "miden")]
        miden: PathBuf,
        #[arg(long, hide = true)]
        no_toolchain_dispatch: bool,
    }
    let options = Options::parse();
    let project = options
        .project
        .unwrap_or_else(|| std::env::current_dir().expect("working directory is unavailable"));
    let project = project.canonicalize().unwrap_or(project);
    let managed = std::env::var_os("MIDENUP_TOOLCHAIN").is_some_and(|value| !value.is_empty())
        && std::env::var_os("MIDEN_SYSROOT").is_some_and(|value| !value.is_empty());
    if !options.no_toolchain_dispatch && (!managed || options.toolchain.is_some()) {
        let channel = options
            .toolchain
            .map(Some)
            .map(Ok)
            .unwrap_or_else(|| miden_lsp::toolchain::project_channel(&project));
        match channel {
            Ok(Some(channel)) => {
                match miden_lsp::toolchain::dispatch(&project, &channel, &options.miden) {
                    Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                    Err(error) => {
                        eprintln!("{error}");
                        std::process::exit(1);
                    }
                }
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
            Ok(None) => (),
        }
    }
    if let Err(error) = std::env::set_current_dir(&project) {
        eprintln!("cannot open project '{}': {error}", project.display());
        std::process::exit(1);
    }
    init_tracing();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("cannot start the language server runtime");
    let exit_code = runtime.block_on(async {
        let (service, socket) = LspService::new(Backend::new);
        let (exit, mut receiver) = tokio::sync::watch::channel(None);
        let service = ExitAwareService {
            inner: service,
            shutdown: Arc::new(AtomicBool::new(false)),
            exit,
        };
        tokio::select! {
            () = Server::new(tokio::io::stdin(), tokio::io::stdout(), socket).serve(service) => 0,
            result = receiver.changed() => {
                if result.is_ok() { receiver.borrow().unwrap_or(1) } else { 1 }
            }
        }
    });
    runtime.shutdown_background();
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

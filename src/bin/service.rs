// SPDX-License-Identifier: MIT
//! enzimos-agent-harness --config FILE --socket PATH --state DIR
use enzimos_agent_harness::service::{Service, bind, config_file};
use std::{path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (mut config, mut socket, mut state) = (None, None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().map(PathBuf::from);
        match flag.as_str() {
            "--config" => config = value,
            "--socket" => socket = value,
            "--state" => state = value,
            _ => return usage(),
        }
    }
    let (Some(config_path), Some(socket), Some(state)) = (config, socket, state) else {
        return usage();
    };
    let config = match config_file(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("agent harness: {}", error.0);
            return ExitCode::FAILURE;
        }
    };
    let absolute = |p: &std::path::Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut protected = vec![
        absolute(&config_path),
        absolute(socket.parent().unwrap_or(&socket)),
    ];
    if let Some(gateway) = config["gateway_socket"]
        .as_str()
        .and_then(|g| std::path::Path::new(g).parent())
    {
        protected.push(absolute(gateway));
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let listener = match bind(&socket) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("agent harness: {}", error.0);
                return ExitCode::FAILURE;
            }
        };
        let service = match Service::new(config, &state, None, &protected) {
            Ok(service) => service,
            Err(error) => {
                let _ = std::fs::remove_file(&socket);
                eprintln!("agent harness: {}", error.0);
                return ExitCode::FAILURE;
            }
        };
        let (Ok(mut term), Ok(mut int)) = (
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
        ) else {
            return ExitCode::FAILURE;
        };
        tokio::select! {
            () = service.clone().serve(listener) => {}
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        service.close();
        let _ = std::fs::remove_file(&socket);
        ExitCode::SUCCESS
    })
}

fn usage() -> ExitCode {
    eprintln!("usage: enzimos-agent-harness --config FILE --socket PATH --state DIR");
    ExitCode::from(2)
}

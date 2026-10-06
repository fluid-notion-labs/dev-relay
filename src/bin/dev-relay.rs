use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("build") => build(&args[1..]),
        _ => usage(),
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn serve(args: &[String]) -> ExitCode {
    let cfg = dev_relay::server::ServeConfig {
        dist_dir: PathBuf::from(flag_value(args, "--dist").unwrap_or_else(|| "relay-dist".into())),
        port: flag_value(args, "--port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(dev_relay::server::DEFAULT_PORT),
    };
    match dev_relay::server::serve(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dev-relay serve: {e}");
            ExitCode::FAILURE
        }
    }
}

fn build(args: &[String]) -> ExitCode {
    let target = match flag_value(args, "--target").as_deref() {
        None | Some("linux") => dev_relay::build::Target::Linux,
        Some("win") | Some("windows") => dev_relay::build::Target::Win,
        Some(other) => {
            eprintln!("unknown --target {other} (linux|win)");
            return ExitCode::FAILURE;
        }
    };
    let cfg = dev_relay::build::BuildConfig {
        project: PathBuf::from(flag_value(args, "--project").unwrap_or_else(|| ".".into())),
        dist: PathBuf::from(flag_value(args, "--dist").unwrap_or_else(|| "relay-dist".into())),
        bin: flag_value(args, "--bin"),
        target,
    };
    match dev_relay::build::run(&cfg) {
        Ok(name) => {
            println!(
                "dev-relay: dropped {name} into {}/incoming/ (serve publishes it)",
                cfg.dist.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("dev-relay build: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: dev-relay serve [--dist DIR] [--port N]");
    eprintln!(
        "       dev-relay build [--target linux|win] [--bin NAME] [--project DIR] [--dist DIR]"
    );
    ExitCode::FAILURE
}

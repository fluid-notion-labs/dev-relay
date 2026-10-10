use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (mine, game_args) = args.split_at(split);

    let url = flag_value(mine, "--url")
        .or_else(|| std::env::var("BILLIARDS_RELAY_URL").ok())
        .filter(|v| !v.is_empty());
    let Some(url) = url else {
        eprintln!("relay-launcher: missing --url (or BILLIARDS_RELAY_URL env)");
        return ExitCode::FAILURE;
    };
    let dir = flag_value(mine, "--dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."))
        });
    let poll = flag_value(mine, "--poll-secs")
        .and_then(|p| p.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(15));
    let verbose = mine.iter().any(|a| a == "--verbose" || a == "-v")
        || std::env::var("BILLIARDS_LAUNCHER_VERBOSE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

    let cfg = dev_relay::launcher::LauncherConfig {
        url,
        dir,
        poll,
        game_args: game_args.iter().skip(1).cloned().collect(),
        verbose,
    };
    match dev_relay::launcher::run(&cfg) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("relay-launcher: {e}");
            ExitCode::FAILURE
        }
    }
}

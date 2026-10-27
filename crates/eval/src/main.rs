//! `eval` binary: offline scoring of try-it classification and brief ranking.
//! No network, no paid calls.

struct Args {
    bookmarks: String,
    labels: String,
    out: Option<String>,
    name: String,
}

fn usage() -> &'static str {
    "usage: eval --bookmarks <jsonl> --labels <jsonl> [--out <md>] [--name <dataset>]"
}

fn take_value(args: &[String], i: usize, flag: &str) -> Result<String, String> {
    args.get(i)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value\n{}", usage()))
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut bookmarks = None;
    let mut labels = None;
    let mut out = None;
    let mut name = "eval".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bookmarks" => {
                i += 1;
                bookmarks = Some(take_value(args, i, "--bookmarks")?);
            }
            "--labels" => {
                i += 1;
                labels = Some(take_value(args, i, "--labels")?);
            }
            "--out" => {
                i += 1;
                out = Some(take_value(args, i, "--out")?);
            }
            "--name" => {
                i += 1;
                name = take_value(args, i, "--name")?;
            }
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
        i += 1;
    }
    Ok(Args {
        bookmarks: bookmarks.ok_or_else(|| format!("missing --bookmarks\n{}", usage()))?,
        labels: labels.ok_or_else(|| format!("missing --labels\n{}", usage()))?,
        out,
        name,
    })
}

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = parse_args(&args)?;
    let report = eval::run(&cfg.bookmarks, &cfg.labels, &cfg.name)?;
    let markdown = eval::render_markdown(&report);
    if let Some(path) = &cfg.out {
        std::fs::write(path, &markdown).map_err(|e| format!("write {path}: {e}"))?;
    }
    print!("{markdown}");
    Ok(())
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("eval: {e}");
        std::process::exit(1);
    }
}

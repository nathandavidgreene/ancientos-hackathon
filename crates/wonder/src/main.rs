//! `wonder [--log PATH] propose <question> | list | approve <id> | deny <id>
//! [reason] | ask <id> | show <id>`: thin binary over [`wonder::cli`]. Uses
//! the shared decision log, so pending questions also show on web-ui's
//! `/approvals` and phone `/m` pages. `ask` calls Muse Wonder only for an
//! approved question, only with `ANCIENTOS_ALLOW_PAID=1` and `WONDER_MODEL`.

/// Run the CLI with the process args and environment.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).ok();
    if let Err(e) = wonder::cli(&args, &env) {
        eprintln!("wonder: {e}");
        std::process::exit(2);
    }
}

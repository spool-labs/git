//! `git-remote-tape`, the Git line-protocol adapter for `tape://` remotes.

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("git-remote-tape {}", tape_git_remote::VERSION);
            return;
        }
        Some("--help" | "-h") => {
            println!("git-remote-tape {}\n\nGit remote helper for tape:// URLs", tape_git_remote::VERSION);
            return;
        }
        _ => {}
    }
    if let Err(error) = tape_git_remote::remote_helper::run_from_env().await {
        eprintln!("git-remote-tape: {error:#}");
        std::process::exit(1);
    }
}

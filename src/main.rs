//! `git-remote-tape`, the Git line-protocol adapter for `tape://` remotes.

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    if let Err(error) = tape_git_remote::remote_helper::run_from_env().await {
        eprintln!("git-remote-tape: {error:#}");
        std::process::exit(1);
    }
}

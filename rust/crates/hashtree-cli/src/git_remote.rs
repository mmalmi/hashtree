//! Git remote helper binary - thin wrapper around git-remote-htree crate
//!
//! This allows `cargo install hashtree-cli` to install both `htree` and `git-remote-htree`

fn main() {
    git_remote_htree::main_entry();
}

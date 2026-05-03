const COMMANDS: &[&str] = &["check", "download_and_install"];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}

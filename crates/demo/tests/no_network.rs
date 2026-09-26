//! The demo cluster must stay network-free: no socket or process APIs in its sources (clippy's
//! disallowed-types list in clippy.toml catches the types; this also catches paths in macros).

use std::path::Path;

#[test]
fn sources_use_no_network_or_process_apis() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        for banned in ["net::", "process::", "Command::new", "connect(", "TcpStream", "UdpSocket", "UnixStream"] {
            assert!(!text.contains(banned), "{} uses {banned}", path.display());
        }
        checked += 1;
    }
    assert!(checked >= 6, "only {checked} source files found");
}

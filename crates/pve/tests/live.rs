//! Read-only smoke test against a real cluster (ignored by default).
//!
//! ```sh
//! PVE_URL=https://pve:8006 PVE_TOKEN_ID='user@realm!name' PVE_TOKEN_SECRET=... \
//! PVE_TEST_VMID=100 [PVE_PIN=AB:CD:...] cargo test -p pve --test live -- --ignored --nocapture
//! ```
//!
//! Only `GET /version`, `GET /cluster/resources` and one console open that reads the 12-byte
//! RFB banner (nothing is ever written to the console). Without `PVE_PIN` a self-signed cluster
//! fails with `UntrustedCert`, and the fingerprint to pin is printed.

use pve::{Client, Endpoint, Error};
use tokio::io::AsyncReadExt;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

#[tokio::test]
#[ignore = "needs a real cluster (PVE_URL, PVE_TOKEN_ID, PVE_TOKEN_SECRET, PVE_TEST_VMID)"]
async fn live_cluster_console_banner() {
    let pin = std::env::var("PVE_PIN").ok().map(|p| pve::parse_fingerprint(&p).expect("PVE_PIN: 64 hex digits"));
    let endpoint = Endpoint {
        url: env("PVE_URL").parse().expect("PVE_URL"),
        token_id: env("PVE_TOKEN_ID"),
        token_secret: env("PVE_TOKEN_SECRET"),
        pinned_sha256: pin,
    };
    let vmid: u32 = env("PVE_TEST_VMID").parse().expect("PVE_TEST_VMID");
    let client = Client::new(&endpoint).unwrap();

    let version = match client.version().await {
        Ok(version) => version,
        Err(Error::UntrustedCert(problem)) => {
            panic!("untrusted certificate, rerun with PVE_PIN={}", pve::format_fingerprint(&problem.sha256))
        }
        Err(e) => panic!("version: {e}"),
    };
    println!("Proxmox VE {} (release {})", version.version, version.release);

    let vms = client.vms().await.unwrap();
    println!("{} guests", vms.len());
    let vm = vms.iter().find(|vm| vm.vmid == vmid).unwrap_or_else(|| panic!("VM {vmid} not in the cluster"));
    assert_eq!(vm.status, "running", "the test VM must be running");

    let (mut stream, password) = client.open_console(&vm.vm_ref()).await.unwrap();
    assert!(!password.is_empty());
    let mut banner = [0u8; 12];
    tokio::time::timeout(std::time::Duration::from_secs(10), stream.read_exact(&mut banner))
        .await
        .expect("banner within 10 s")
        .unwrap();
    println!("banner {:?}", String::from_utf8_lossy(&banner));
    assert!(banner.starts_with(b"RFB 00"), "{banner:?}");
    drop(stream);
}

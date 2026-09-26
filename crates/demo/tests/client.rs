//! The demo's RFB server against the app's real RFB client, over the in-memory pipe.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rfb::{ClientInput, Config, Event, SharedFramebuffer};
use tokio::sync::mpsc;

struct Console {
    fb: SharedFramebuffer,
    events: Arc<Mutex<Vec<Event>>>,
    input: mpsc::UnboundedSender<ClientInput>,
}

fn cluster() -> Arc<demo::Cluster> {
    demo::Cluster::new(demo::Fonts {
        regular: include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"),
        bold: include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf"),
    })
    .unwrap()
}

async fn open(cluster: &demo::Cluster, vmid: u32) -> Console {
    let stream = cluster.open_console(vmid).await.unwrap();
    let fb = SharedFramebuffer::default();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let notify: rfb::Notify = Arc::new(move |e| sink.lock().unwrap().push(e));
    let (input, rx) = mpsc::unbounded_channel();
    tokio::spawn(rfb::run(stream, Config::default(), Arc::clone(&fb), rx, notify));
    let console = Console { fb, events, input };
    wait_for("the first update", || console.events.lock().unwrap().contains(&Event::Updated)).await;
    console
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Type `text`; `scancodes` picks QEMU extended key events (else plain RFB key events).
fn type_text(c: &Console, text: &str, scancodes: bool) {
    for ch in text.chars() {
        let keysym = if ch == '\n' { rfb::keysym::RETURN } else { ch as u32 };
        let qnum = if scancodes { 0x1e } else { 0 }; // the server reads the keysym only
        for down in [true, false] {
            c.input.send(ClientInput::Key { keysym, qnum, down }).unwrap();
        }
    }
}

fn pixels(c: &Console) -> Vec<u8> {
    c.fb.lock().unwrap().pixels().to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_real_client_types_and_sees_the_screen() {
    let cluster = cluster();
    let a = open(&cluster, 101).await;
    assert_eq!(
        a.events.lock().unwrap().first(),
        Some(&Event::Connected { width: 960, height: 600, name: "web-01".into() })
    );
    assert_eq!(demo::SCREEN, (960, 600));

    type_text(&a, "host", false);
    tokio::time::sleep(Duration::from_millis(100)).await; // the key announcement has arrived by now
    type_text(&a, "name\n", true);
    wait_for("hostname output", || cluster.screen_text(101).unwrap().ends_with("# hostname\nweb-01\nroot@web-01:~#"))
        .await;
    // enough output to scroll (CopyRect) a few times
    for _ in 0..3 {
        type_text(&a, "ip a\n", true);
    }
    wait_for("ip output", || {
        let text = cluster.screen_text(101).unwrap();
        !text.contains("hostname") && text.ends_with("preferred_lft forever\nroot@web-01:~#")
    })
    .await;

    // a second client gets one full update: the first one's incremental picture must match it
    let b = open(&cluster, 101).await;
    wait_for("identical screens", || pixels(&a) == pixels(&b)).await;
    let lit = pixels(&b).as_chunks::<4>().0.iter().filter(|p| p[0] > 128).count();
    assert!(lit > 5_000, "text is drawn ({lit} bright pixels)");
    assert!(pixels(&b).as_chunks::<4>().0.iter().all(|p| p[3] == 255));

    // powering off closes both consoles
    type_text(&a, "poweroff\n", true);
    wait_for("the VM to stop", || cluster.guests().iter().any(|g| g.vmid == 101 && !g.running)).await;
    wait_for("the consoles to close", || a.input.is_closed() && b.input.is_closed()).await;
}

//! One live console: `vncproxy` + websocket + RFB session on the tokio runtime (or, for the demo
//! cluster, an in-process pipe to a simulated VM).

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::backend::Backend;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Live,
    /// The session is over: `Ok` = closed by the server, `Err` = why it failed.
    Ended(Result<(), String>),
}

/// Dropping a `Console` aborts the session (closes the websocket).
pub struct Console {
    pub fb: rfb::SharedFramebuffer,
    input: UnboundedSender<rfb::ClientInput>,
    state: Arc<Mutex<ConnState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Console {
    pub fn open(rt: &tokio::runtime::Handle, backend: Backend, vm: pve::VmRef, ctx: egui::Context) -> Self {
        let fb = rfb::SharedFramebuffer::default();
        let (input, rx) = unbounded_channel();
        let state = Arc::new(Mutex::new(ConnState::Connecting));

        let notify: rfb::Notify = {
            let state = Arc::clone(&state);
            let ctx = ctx.clone();
            Arc::new(move |event| {
                if matches!(event, rfb::Event::Connected { .. }) {
                    *state.lock().unwrap_or_else(PoisonError::into_inner) = ConnState::Live;
                }
                ctx.request_repaint();
            })
        };

        let task = rt.spawn({
            let fb = Arc::clone(&fb);
            let state = Arc::clone(&state);
            async move {
                let result = async {
                    match backend {
                        Backend::Pve(client) => {
                            let (stream, password) = client.open_console(&vm).await.map_err(|e| e.to_string())?;
                            rfb::run(stream, rfb::Config::with_password(password), fb, rx, notify).await
                        }
                        Backend::Demo(cluster) => {
                            let stream = cluster.open_console(vm.vmid).await?;
                            rfb::run(stream, rfb::Config::default(), fb, rx, notify).await
                        }
                    }
                    .map_err(|e| e.to_string())
                }
                .await;
                if let Err(e) = &result {
                    tracing::debug!(vmid = vm.vmid, "console ended: {e}");
                }
                *state.lock().unwrap_or_else(PoisonError::into_inner) = ConnState::Ended(result);
                ctx.request_repaint();
            }
        });

        Self { fb, input, state, task }
    }

    pub fn state(&self) -> ConnState {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn send(&self, input: rfb::ClientInput) {
        // a closed session simply drops input
        let _ = self.input.send(input);
    }

    pub fn sender(&self) -> UnboundedSender<rfb::ClientInput> {
        self.input.clone()
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.task.abort();
    }
}

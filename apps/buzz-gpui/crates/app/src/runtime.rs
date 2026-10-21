//! Tokio runs networking and the agent host; GPUI runs the UI. Futures that
//! need tokio are spawned here and awaited from GPUI tasks.

use gpui_kit::{App, Global};

pub struct Runtime(tokio::runtime::Runtime);

impl Global for Runtime {}

impl Runtime {
    pub fn init(cx: &mut App) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("buzz-io")
            .enable_all()
            .build()
            .expect("start the tokio runtime");
        cx.set_global(Self(runtime));
    }

    pub fn handle(cx: &App) -> tokio::runtime::Handle {
        cx.global::<Self>().0.handle().clone()
    }
}

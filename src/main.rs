//! Native PipeWire multi-output router.

use gpui_kit::*;
use pw_matrix_router::{
    audio::AudioEngine,
    ui::{self, RouterView},
};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM},
    iterator::{Handle, Signals},
};
use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

/// GPUI only waits 200 ms for quit observers. This owner finishes audio cleanup
/// after its event loop returns, and also on an unwinding startup/render panic.
struct ShutdownGuard {
    engine: Arc<AudioEngine>,
    signals: Handle,
    signal_worker: Option<JoinHandle<()>>,
}

impl ShutdownGuard {
    fn new(engine: Arc<AudioEngine>) -> anyhow::Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
        let handle = signals.handle();
        let signal_engine = engine.clone();
        let worker = thread::Builder::new()
            .name("router-signals".into())
            .spawn(move || {
                if let Some(signal) = signals.forever().next() {
                    if let Err(error) = signal_engine.shutdown() {
                        eprintln!("Audio shutdown: {error:#}");
                    }
                    std::process::exit(128 + signal);
                }
            })?;
        Ok(Self {
            engine,
            signals: handle,
            signal_worker: Some(worker),
        })
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.signals.close();
        if let Some(worker) = self.signal_worker.take() {
            let _ = worker.join();
        }
        if let Err(error) = self.engine.shutdown() {
            eprintln!("Audio shutdown: {error:#}");
        }
    }
}

fn main() -> anyhow::Result<()> {
    let engine = AudioEngine::new();
    let _shutdown = ShutdownGuard::new(engine.clone())?;

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            ui::init(cx);

            let start_engine = engine.clone();
            cx.background_spawn(async move {
                if let Err(error) = start_engine.start() {
                    eprintln!("Audio startup: {error:#}");
                }
            })
            .detach();

            let quit_engine = engine.clone();
            cx.on_app_quit(move |_| {
                let engine = quit_engine.clone();
                // Independent of the foreground task's short quit deadline.
                thread::spawn(move || {
                    if let Err(error) = engine.shutdown() {
                        eprintln!("Audio shutdown: {error:#}");
                    }
                });
                async {}
            })
            .detach();
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            // Pixel sizes are native window-system bounds; internal UI uses rem.
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1200.), px(800.)),
                    cx,
                ))),
                window_min_size: Some(size(px(1000.), px(650.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("PW-Matrix — Audio router".into()),
                    ..Default::default()
                }),
                app_id: Some("pw-matrix-router".into()),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| RouterView::new(engine, window, cx))
            })
            .expect("failed to open PW-Matrix window");
        });
    Ok(())
}

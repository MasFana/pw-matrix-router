use super::{RouterView, sync_slider};
use crate::audio::{AudioEngine, EndpointSnapshot, EngineSnapshot};
use gpui_kit::test::{TestEventExt, TestQueryExt, TestWindowExt};
use gpui_kit::{
    AppContext, Bounds, Entity, MouseButton, TestAppContext, WindowBounds, WindowOptions, point,
    px, size,
};

fn open(
    cx: &mut TestAppContext,
) -> (
    gpui_kit::AnyWindowHandle,
    Entity<RouterView>,
    std::sync::Arc<AudioEngine>,
) {
    cx.update(crate::ui::init);
    let engine = AudioEngine::new();
    let shared = engine.clone();
    let (handle, view) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1200.), px(850.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| RouterView::new(shared, window, cx)),
        )
        .expect("open router test window")
    });
    (handle, view, engine)
}

fn endpoints() -> EngineSnapshot {
    let mut snapshot = EngineSnapshot::default();
    snapshot.capturing = true;
    snapshot.running = true;
    snapshot.master_volume = 1.;
    snapshot.target_floor_ms = 12;
    snapshot.endpoints = ["sink-a", "sink-b", "sink-c"]
        .into_iter()
        .map(|id| {
            let mut endpoint = EndpointSnapshot::default();
            endpoint.id = id.to_owned();
            endpoint.name = id.to_owned();
            endpoint.volume = 1.;
            endpoint.sample_rate = 48000;
            endpoint.channels = 2;
            endpoint.bits = 32;
            endpoint
        })
        .collect();
    snapshot
}

#[gpui_kit::test]
fn real_controls_command_engine_and_keep_numeric_keys_local(cx: &mut TestAppContext) {
    let (handle, view, engine) = open(cx);
    let initial = engine.snapshot();
    assert!(!initial.capturing, "view construction must not start audio");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("refresh").disabled(), Some(true));
        window.click("master-mute", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_ne!(engine.snapshot().master_muted, initial.master_muted);
    cx.update_window(handle, |_, window, cx| window.click("auto-sync", cx))
        .unwrap();
    cx.run_until_parked();
    assert_ne!(engine.snapshot().auto_sync, initial.auto_sync);
    cx.update_window(handle, |_, window, cx| window.press("space", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        engine.snapshot().auto_sync,
        initial.auto_sync,
        "Space belongs to the focused switch"
    );
    cx.update_window(handle, |_, window, cx| {
        window.within("floor-input").click("input-state", cx);
        window.press("ctrl-a", cx);
        window.input("20", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(engine.snapshot().target_floor_ms, 20);
    cx.update_window(handle, |_, window, cx| window.press("up", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        engine.snapshot().target_floor_ms,
        22,
        "Arrow keys step the number input, not matrix selection"
    );
    assert_eq!(engine.snapshot().master_volume, initial.master_volume);
    cx.update_window(handle, |_, window, cx| {
        window.press("ctrl-a", cx);
        window.input("101", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        engine.snapshot().target_floor_ms,
        22,
        "Invalid input must not command the engine"
    );
    cx.update(|cx| assert!(view.read(cx).floor_error.is_some()));
}

#[gpui_kit::test]
fn matrix_selection_survives_reordering_and_disconnect(cx: &mut TestAppContext) {
    let (handle, view, _) = open(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| view.apply_snapshot(endpoints(), window, cx));
        window.render_frame(cx);
        window.press("j", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(view.read(cx).selected_id.as_deref(), Some("sink-b")));
    cx.update_window(handle, |_, window, cx| {
        let mut snapshot = endpoints();
        snapshot.endpoints.rotate_left(1);
        view.update(cx, |view, cx| view.apply_snapshot(snapshot, window, cx));
        window.render_frame(cx);
        assert_eq!(view.read(cx).selected_id.as_deref(), Some("sink-b"));
        assert_eq!(view.read(cx).table.read(cx).selected_row(), Some(0));
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        let mut snapshot = endpoints();
        snapshot
            .endpoints
            .retain(|endpoint| endpoint.id != "sink-b");
        view.update(cx, |view, cx| view.apply_snapshot(snapshot, window, cx));
        window.render_frame(cx);
        assert_eq!(view.read(cx).selected_id.as_deref(), Some("sink-a"));
        assert_eq!(view.read(cx).table.read(cx).selected_row(), Some(0));
    })
    .unwrap();
}

#[gpui_kit::test]
fn live_slider_drag_is_not_rolled_back_by_snapshot_sync(cx: &mut TestAppContext) {
    let (handle, view, engine) = open(cx);
    let position = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let track = window
                .within("master-volume")
                .find("slider-bar-container")
                .bounds();
            let position = point(track.left() + track.size.width * 0.6, track.center().y);
            window.pointer_down(position, MouseButton::Left, cx);
            position
        })
        .unwrap();
    cx.run_until_parked();
    let volume = engine.snapshot().master_volume;
    assert!((volume - 0.9).abs() < 0.03);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| {
            let mut snapshot = engine.snapshot();
            snapshot.master_volume = 0.2;
            view.apply_snapshot(snapshot, window, cx);
            assert!((view.master_volume.read(cx).value().end() - volume).abs() < 0.0001);
        });
        window.pointer_up(position, MouseButton::Left, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| {
            sync_slider(&view.master_volume, 0.4, window, cx)
        });
        window.render_frame(cx);
        assert_eq!(view.read(cx).master_volume.read(cx).value().end(), 0.4);
    })
    .unwrap();
    assert!(
        (engine.snapshot().master_volume - volume).abs() < 0.0001,
        "Programmatic synchronization must not feed back into engine commands"
    );
}

#[gpui_kit::test]
fn matrix_shortcuts_preserve_master_gain_mute_and_delay_controls(cx: &mut TestAppContext) {
    let (handle, view, engine) = open(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| view.apply_snapshot(endpoints(), window, cx));
        window.render_frame(cx);
        window.press("]", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!((engine.snapshot().master_volume - 1.02).abs() < 0.0001);
    cx.update_window(handle, |_, window, cx| window.press("[", cx))
        .unwrap();
    cx.run_until_parked();
    assert!((engine.snapshot().master_volume - 1.0).abs() < 0.0001);
    cx.update_window(handle, |_, window, cx| window.press("shift-m", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(engine.snapshot().master_muted);
    let floor = engine.snapshot().target_floor_ms;
    cx.update_window(handle, |_, window, cx| window.press("-", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(engine.snapshot().target_floor_ms, floor - 2);
    cx.update_window(handle, |_, window, cx| window.press("shift-=", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(engine.snapshot().target_floor_ms, floor);
    let sync = engine.snapshot().auto_sync;
    cx.update_window(handle, |_, window, cx| window.press("a", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(engine.snapshot().auto_sync, !sync);
}

#[gpui_kit::test]
fn matrix_pointer_selection_updates_the_endpoint_inspector(cx: &mut TestAppContext) {
    let (handle, view, _) = open(cx);
    cx.update_window(handle, |_, window, cx| {
        let mut snapshot = endpoints();
        snapshot.endpoints[1].volume = 0.35;
        view.update(cx, |view, cx| view.apply_snapshot(snapshot, window, cx));
        window.render_frame(cx);
        window.click("sink-b", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.selected_id.as_deref(), Some("sink-b"));
        assert_eq!(view.table.read(cx).selected_row(), Some(1));
        assert!((view.device_volume.read(cx).value().end() - 0.35).abs() < 0.0001);
    });
}

#[gpui_kit::test]
fn controls_follow_outputs_and_errors_open_only_in_audio_details(cx: &mut TestAppContext) {
    let (handle, view, _) = open(cx);
    let warning = "Some application streams could not be moved";
    cx.update_window(handle, |_, window, cx| {
        let mut snapshot = endpoints();
        view.update(cx, |view, cx| {
            view.apply_snapshot(snapshot.clone(), window, cx)
        });
        window.render_frame(cx);
        let footer_height = window.find("router-status").bounds().size.height;
        snapshot.error = Some(warning.into());
        view.update(cx, |view, cx| view.apply_snapshot(snapshot, window, cx));
        window.render_frame(cx);
        let matrix = window.find("matrix").bounds();
        let controls = window.find("inspector").bounds();
        assert_eq!(matrix.left(), controls.left());
        assert_eq!(matrix.size.width, controls.size.width);
        assert!(controls.top() >= matrix.bottom());
        assert!(window.find("device-route").bounds().top() >= matrix.bottom());
        assert_eq!(
            window.find("router-status").bounds().size.height,
            footer_height
        );
        window.click("sink-b", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(view.read(cx).selected_id.as_deref(), Some("sink-b")));
    cx.update_window(handle, |_, window, cx| {
        window.click("audio-details", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("audio-detail-error").label().as_deref(),
            Some(warning)
        );
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("audio-detail-error").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn stereo_levels_and_balance_keep_controls_and_meter_bounds_stable(cx: &mut TestAppContext) {
    let (handle, view, _) = open(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| view.apply_snapshot(endpoints(), window, cx));
        window.render_frame(cx);
        let ids = [
            "left-level",
            "right-level",
            "stereo-balance",
            "device-route",
            "inspector",
        ];
        let initial = ids.map(|id| window.find(id).bounds());
        assert_eq!(window.find_all_by_role(gpui_kit::Role::Meter).len(), 3);
        assert_eq!(initial[0].size.width, initial[1].size.width);
        assert!(initial[0].bottom() <= initial[1].top());
        assert!(initial[1].bottom() <= initial[2].top());
        for (left, right, balance) in [
            (0., 0., 0.),
            (1e-20, 1e-12, -1.),
            (0.8, 0.02, 1.),
            (1.5, 0.4, 0.65),
            (f64::NAN, f64::INFINITY, f64::INFINITY),
        ] {
            let mut snapshot = endpoints();
            snapshot.telemetry.rms_l = left;
            snapshot.telemetry.rms_r = right;
            snapshot.telemetry.peak_l = left * 1.4;
            snapshot.telemetry.peak_r = right * 1.4;
            snapshot.telemetry.peak_hold_l = 1.;
            snapshot.telemetry.peak_hold_r = 0.25;
            snapshot.telemetry.balance = balance;
            view.update(cx, |view, cx| view.apply_snapshot(snapshot, window, cx));
            window.render_frame(cx);
            assert_eq!(ids.map(|id| window.find(id).bounds()), initial);
            assert_eq!(window.find_all_by_role(gpui_kit::Role::Meter).len(), 3);
        }
    })
    .unwrap();
}

#[test]
fn telemetry_changes_do_not_invalidate_router_model() {
    let previous = endpoints();
    let mut next = previous.clone();
    next.telemetry.rms_l = 0.5;
    next.telemetry.balance = -0.5;
    next.telemetry.spectrum[3] = 0.8;
    assert!(super::same_model(&previous, &next));
    next.endpoints[0].underruns += 1;
    assert!(!super::same_model(&previous, &next));
    next.endpoints[0].underruns -= 1;
    next.master_muted = !previous.master_muted;
    assert!(!super::same_model(&previous, &next));
}

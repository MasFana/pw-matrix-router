//! Commands scoped to the output matrix, except the global quit accelerator.

use gpui_kit::{App, KeyBinding, actions};

actions!(
    AudioRouterActions,
    [
        SelectUp,
        SelectDown,
        ToggleTarget,
        ToggleDeviceMute,
        ToggleMasterMute,
        IncreaseVolume,
        DecreaseVolume,
        IncreaseMasterVolume,
        DecreaseMasterVolume,
        TightenFloor,
        RelaxFloor,
        ToggleAutoSync,
        DecreaseOffset,
        IncreaseOffset,
        RefreshDevices,
        Quit,
    ]
);

/// Install after `gpui_kit::init`, before opening the application window.
pub(super) fn bind_keys(cx: &mut App) {
    let matrix = Some("Matrix > DataTable");
    cx.bind_keys([
        KeyBinding::new("k", SelectUp, matrix),
        KeyBinding::new("j", SelectDown, matrix),
        KeyBinding::new("space", ToggleTarget, matrix),
        KeyBinding::new("enter", ToggleTarget, matrix),
        KeyBinding::new("m", ToggleDeviceMute, matrix),
        KeyBinding::new("shift-m", ToggleMasterMute, matrix),
        KeyBinding::new("left", DecreaseVolume, matrix),
        KeyBinding::new("h", DecreaseVolume, matrix),
        KeyBinding::new("right", IncreaseVolume, matrix),
        KeyBinding::new("l", IncreaseVolume, matrix),
        KeyBinding::new("[", DecreaseMasterVolume, matrix),
        KeyBinding::new("]", IncreaseMasterVolume, matrix),
        KeyBinding::new("-", TightenFloor, matrix),
        KeyBinding::new("shift--", TightenFloor, matrix),
        KeyBinding::new("=", RelaxFloor, matrix),
        KeyBinding::new("shift-=", RelaxFloor, matrix),
        KeyBinding::new("a", ToggleAutoSync, matrix),
        KeyBinding::new(",", DecreaseOffset, matrix),
        KeyBinding::new(".", IncreaseOffset, matrix),
        KeyBinding::new("r", RefreshDevices, matrix),
        KeyBinding::new("q", Quit, matrix),
        KeyBinding::new("ctrl-c", Quit, matrix),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

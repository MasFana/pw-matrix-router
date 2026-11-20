use std::{sync::Arc, time::Duration};

use gpui_kit::component::{
    ActiveTheme, Disableable, Sizable, Size, WindowExt,
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState, NumberInput},
    scroll::ScrollableElement,
    slider::{Slider, SliderEvent, SliderState},
    spinner::Spinner,
    switch::Switch,
    table::{DataTable, TableEvent, TableState},
};
use gpui_kit::{
    AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Window,
    div,
};

use super::actions::*;
use crate::{
    audio::{AudioEngine, EndpointSnapshot, EngineSnapshot},
    components::{device_row::OutputTable, spectrum_display::TelemetryView},
};

#[derive(Clone, Copy)]
enum Operation {
    Refresh,
    Retry,
}

/// Owns the native matrix, retained controls, and snapshot polling lifecycle.
/// Construction does not start audio; the application starts the engine in the background.
pub struct RouterView {
    engine: Arc<AudioEngine>,
    snapshot: EngineSnapshot,
    selected_id: Option<String>,
    table: Entity<TableState<OutputTable>>,
    telemetry: Entity<TelemetryView>,
    master_volume: Entity<SliderState>,
    device_volume: Entity<SliderState>,
    floor: Entity<InputState>,
    offset: Entity<InputState>,
    floor_synced: Option<u32>,
    offset_synced: Option<i32>,
    master_dragging: bool,
    device_drag_id: Option<String>,
    floor_error: Option<SharedString>,
    offset_error: Option<SharedString>,
    operation: Option<Operation>,
    operation_error: Option<String>,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
    _operation_task: Option<Task<()>>,
}

impl RouterView {
    pub fn new(engine: Arc<AudioEngine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snapshot = engine.snapshot();
        let selected_id = snapshot
            .endpoints
            .first()
            .map(|endpoint| endpoint.id.clone());
        let selected = snapshot.endpoints.first();
        let mut delegate = OutputTable::new(window.rem_size());
        delegate.replace(&[], &snapshot.endpoints);
        delegate.loading = !snapshot.running && !snapshot.startup_failed;
        if snapshot.error.is_some() {
            delegate.empty_message =
                "Output discovery failed. Retry audio startup to reconnect.".into();
        }
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .col_selectable(false)
                .col_movable(false)
                .sortable(false)
                .loop_selection(false)
        });
        if selected_id.is_some() {
            table.update(cx, |table, cx| table.set_selected_row(0, cx));
        }
        table.focus_handle(cx).focus(window, cx);
        let master_volume = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.5)
                .step(0.02)
                .default_value(snapshot.master_volume)
        });
        let device_volume = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.5)
                .step(0.02)
                .default_value(selected.map_or(1., |endpoint| endpoint.volume))
        });
        let floor = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(snapshot.target_floor_ms.to_string())
                .step(2.)
                .min(6.)
                .max(100.)
                .validate(|text, _| text.bytes().all(|byte| byte.is_ascii_digit()))
        });
        let offset = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(
                    selected
                        .map_or(0, |endpoint| endpoint.offset_ms)
                        .to_string(),
                )
                .step(2.)
                .min(0.)
                .max(1000.)
                .validate(|text, _| text.bytes().all(|byte| byte.is_ascii_digit()))
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &table,
                window,
                |this, table, event, window, cx| match event {
                    TableEvent::SelectRow(ix) => {
                        let id = table.read(cx).delegate().id_at(*ix).map(str::to_owned);
                        if id != this.selected_id {
                            this.selected_id = id;
                            this.offset_error = None;
                            this.sync_controls(true, window, cx);
                            cx.notify();
                        }
                    }
                    TableEvent::ClearSelection => {
                        this.selected_id = None;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
            cx.subscribe(&master_volume, |this, _, event: &SliderEvent, cx| {
                match event {
                    SliderEvent::Change(value) => {
                        this.master_dragging = true;
                        this.engine.set_master_volume(value.end());
                    }
                    SliderEvent::Release(_) => {
                        this.master_dragging = false;
                    }
                }
                cx.notify();
            }),
            cx.subscribe(&device_volume, |this, _, event: &SliderEvent, cx| {
                match event {
                    SliderEvent::Change(value) => {
                        if this.device_drag_id.is_none() {
                            this.device_drag_id = this.selected_id.clone();
                        }
                        if let Some(id) = &this.device_drag_id {
                            this.engine.set_device_volume(id, value.end());
                        }
                    }
                    SliderEvent::Release(_) => {
                        this.device_drag_id = None;
                    }
                }
                cx.notify();
            }),
            cx.subscribe_in(
                &floor,
                window,
                |this, state, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                        let value = state.read(cx).value().parse::<u32>();
                        match value {
                            Ok(ms) if (6..=100).contains(&ms) => {
                                this.floor_error = None;
                                this.engine.set_target_floor(ms);
                                this.apply_snapshot(this.engine.snapshot(), window, cx);
                            }
                            _ => {
                                this.floor_error =
                                    Some("Enter a whole number from 6 to 100 ms.".into())
                            }
                        }
                        cx.notify();
                    } else if matches!(event, InputEvent::Blur) {
                        this.floor_error = None;
                        this.floor_synced = None;
                        this.sync_controls(false, window, cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &offset,
                window,
                |this, state, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                        let value = state.read(cx).value().parse::<i32>();
                        match value {
                            Ok(ms) if (0..=1000).contains(&ms) => {
                                this.offset_error = None;
                                if let Some(id) = &this.selected_id {
                                    this.engine.set_device_offset(id, ms);
                                }
                                this.apply_snapshot(this.engine.snapshot(), window, cx);
                            }
                            _ => {
                                this.offset_error =
                                    Some("Enter a whole number from 0 to 1000 ms.".into())
                            }
                        }
                        cx.notify();
                    } else if matches!(event, InputEvent::Blur) {
                        this.offset_error = None;
                        this.offset_synced = None;
                        this.sync_controls(false, window, cx);
                        cx.notify();
                    }
                },
            ),
        ];
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| {
                        this.apply_snapshot(this.engine.snapshot(), window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let floor_synced = Some(snapshot.target_floor_ms);
        let offset_synced = selected.map(|endpoint| endpoint.offset_ms);
        let telemetry = cx.new(|_| TelemetryView::new(snapshot.telemetry));
        Self {
            engine,
            snapshot,
            selected_id,
            table,
            telemetry,
            master_volume,
            device_volume,
            floor,
            offset,
            master_dragging: false,
            device_drag_id: None,
            floor_synced,
            offset_synced,
            floor_error: None,
            offset_error: None,
            operation: None,
            operation_error: None,
            _subscriptions: subscriptions,
            _poll: poll,
            _operation_task: None,
        }
    }

    fn selected(&self) -> Option<&EndpointSnapshot> {
        let id = self.selected_id.as_deref()?;
        self.snapshot
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == id)
    }

    fn apply_snapshot(
        &mut self,
        snapshot: EngineSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snapshot.telemetry != snapshot.telemetry {
            self.telemetry.update(cx, |telemetry, cx| {
                telemetry.set_snapshot(snapshot.telemetry, cx);
            });
        }
        let previous = std::mem::replace(&mut self.snapshot, snapshot);
        if same_model(&previous, &self.snapshot) {
            self.sync_controls(false, window, cx);
            return;
        }
        let table_changed = !same_table(&previous.endpoints, &self.snapshot.endpoints);
        let loading = !self.snapshot.running && !self.snapshot.startup_failed;
        let loading_changed = loading != (!previous.running && !previous.startup_failed);
        let error_changed = previous.error != self.snapshot.error;
        let selection_missing = self.selected_id.as_ref().is_some_and(|id| {
            !self
                .snapshot
                .endpoints
                .iter()
                .any(|endpoint| &endpoint.id == id)
        });
        let selection_changed = selection_missing
            || (self.selected_id.is_none()
                && previous.endpoints.is_empty()
                && !self.snapshot.endpoints.is_empty());
        if selection_changed {
            self.selected_id = self
                .snapshot
                .endpoints
                .first()
                .map(|endpoint| endpoint.id.clone());
        }
        if table_changed || loading_changed || error_changed {
            let selected_ix = self.selected_id.as_ref().and_then(|id| {
                self.snapshot
                    .endpoints
                    .iter()
                    .position(|endpoint| &endpoint.id == id)
            });
            self.table.update(cx, |table, cx| {
                let delegate = table.delegate_mut();
                if table_changed {
                    delegate.replace(&previous.endpoints, &self.snapshot.endpoints);
                }
                delegate.loading = loading;
                delegate.empty_message = if self.snapshot.error.is_some() {
                    "Output discovery failed. Retry startup or refresh when audio is available."
                        .into()
                } else {
                    "No hardware outputs found. Connect an output and refresh.".into()
                };
                match selected_ix {
                    Some(ix) if table.selected_row() != Some(ix) => table.set_selected_row(ix, cx),
                    None if table.selected_row().is_some() => table.clear_selection(cx),
                    _ => {}
                }
                cx.notify();
            });
        }
        self.sync_controls(selection_changed, window, cx);
        cx.notify();
    }

    fn sync_controls(
        &mut self,
        selection_changed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.master_dragging {
            sync_slider(&self.master_volume, self.snapshot.master_volume, window, cx);
        }
        if !self.floor.focus_handle(cx).is_focused(window)
            && self.floor_synced != Some(self.snapshot.target_floor_ms)
        {
            sync_number(
                &self.floor,
                self.snapshot.target_floor_ms.to_string(),
                window,
                cx,
            );
            self.floor_synced = Some(self.snapshot.target_floor_ms);
        }
        if let Some((volume, offset)) = self
            .selected()
            .map(|endpoint| (endpoint.volume, endpoint.offset_ms))
        {
            if self.device_drag_id.is_none() {
                sync_slider(&self.device_volume, volume, window, cx);
            }
            if selection_changed
                || (!self.offset.focus_handle(cx).is_focused(window)
                    && self.offset_synced != Some(offset))
            {
                sync_number(&self.offset, offset.to_string(), window, cx);
                self.offset_synced = Some(offset);
            }
        }
    }

    fn run_operation(&mut self, operation: Operation, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation.is_some() {
            return;
        }
        match operation {
            Operation::Retry if !self.snapshot.startup_failed => return,
            Operation::Refresh if !self.snapshot.running => return,
            _ => {}
        }
        self.operation = Some(operation);
        self.operation_error = None;
        let engine = self.engine.clone();
        self._operation_task = Some(cx.spawn_in(window, async move |this, cx| {
            let (result, snapshot) = cx
                .background_spawn(async move {
                    let result = match operation {
                        Operation::Refresh => engine.refresh_devices(),
                        Operation::Retry => engine.start(),
                    };
                    (result, engine.snapshot())
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.operation = None;
                this.operation_error = result.err().map(|error| error.to_string());
                this.apply_snapshot(snapshot, window, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn toggle_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snapshot.running {
            if let Some(id) = &self.selected_id {
                self.engine.toggle_target(id);
            }
            self.apply_snapshot(self.engine.snapshot(), window, cx);
        }
    }

    fn toggle_device_mute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = &self.selected_id {
            self.engine.toggle_device_mute(id);
        }
        self.apply_snapshot(self.engine.snapshot(), window, cx);
    }

    fn toggle_master_mute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.engine.toggle_master_mute();
        self.apply_snapshot(self.engine.snapshot(), window, cx);
    }

    fn set_auto_sync(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.engine.set_auto_sync(enabled);
        self.apply_snapshot(self.engine.snapshot(), window, cx);
    }

    fn select_relative(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.snapshot.endpoints.len();
        if len == 0 {
            return;
        }
        let ix = self.table.read(cx).selected_row().unwrap_or(0);
        let next = ix.saturating_add_signed(delta).min(len - 1);
        self.table
            .update(cx, |table, cx| table.set_selected_row(next, cx));
    }

    fn adjust_volume(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(endpoint) = self.selected() {
            self.engine
                .set_device_volume(&endpoint.id, endpoint.volume + delta);
        }
        self.apply_snapshot(self.engine.snapshot(), window, cx);
    }

    fn adjust_master(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.engine
            .set_master_volume(self.snapshot.master_volume + delta);
        self.apply_snapshot(self.engine.snapshot(), window, cx);
    }

    fn adjust_floor(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        self.engine
            .set_target_floor(self.snapshot.target_floor_ms.saturating_add_signed(delta));
        self.apply_snapshot(self.engine.snapshot(), window, cx);
    }

    fn adjust_offset(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(endpoint) = self.selected() {
            self.engine
                .set_device_offset(&endpoint.id, endpoint.offset_ms + delta);
        }
        self.apply_snapshot(self.engine.snapshot(), window, cx);
    }

    fn render_master(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().text_lg().child("PipeWire Matrix"))
                    .child(div().flex_1())
                    .child(
                        Button::new("audio-details")
                            .ghost()
                            .label("Audio details…")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_audio_details(window, cx)
                            })),
                    )
                    .child(
                        Button::new("refresh")
                            .ghost()
                            .label("Refresh outputs")
                            .disabled(self.operation.is_some() || !self.snapshot.running)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run_operation(Operation::Refresh, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .min_w_0()
                            .child(format!(
                                "Master volume  {:.0}%",
                                self.master_volume.read(cx).value().end() * 100.
                            ))
                            .child(
                                div().id("master-volume").child(
                                    Slider::new(&self.master_volume)
                                        .accessibility_label("Master volume"),
                                ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(gain_summary(
                                        self.master_volume.read(cx).value().end(),
                                        self.snapshot.master_muted,
                                    )),
                            ),
                    )
                    .child(
                        Button::new("master-mute")
                            .outline()
                            .label(if self.snapshot.master_muted {
                                "Unmute master"
                            } else {
                                "Mute master"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_master_mute(window, cx)
                            })),
                    )
                    .child(
                        Switch::new("auto-sync")
                            .label("Auto-sync")
                            .checked(self.snapshot.auto_sync)
                            .tooltip("Aligns estimated playback buffers, not measured DAC phase")
                            .on_change(cx.listener(|this, value, window, cx| {
                                this.set_auto_sync(*value, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .w_40()
                            .child("Buffer floor")
                            .child(
                                div().id("floor-input").child(
                                    NumberInput::new(&self.floor)
                                        .accessibility_label("Buffer floor in milliseconds")
                                        .suffix("ms"),
                                ),
                            )
                            .children(self.floor_error.as_ref().map(|error| {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().danger)
                                    .child(error.clone())
                            })),
                    ),
            )
    }

    fn render_matrix(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().id("matrix").test_support().aria_label("Output matrix").key_context("Matrix").w_full().flex_shrink_0().flex().flex_col().min_w_0()
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.select_relative(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.select_relative(1, cx)))
            .on_action(cx.listener(|this, _: &ToggleTarget, window, cx| this.toggle_target(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleDeviceMute, window, cx| this.toggle_device_mute(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleMasterMute, window, cx| this.toggle_master_mute(window, cx)))
            .on_action(cx.listener(|this, _: &IncreaseVolume, window, cx| this.adjust_volume(0.02, window, cx)))
            .on_action(cx.listener(|this, _: &DecreaseVolume, window, cx| this.adjust_volume(-0.02, window, cx)))
            .on_action(cx.listener(|this, _: &IncreaseMasterVolume, window, cx| this.adjust_master(0.02, window, cx)))
            .on_action(cx.listener(|this, _: &DecreaseMasterVolume, window, cx| this.adjust_master(-0.02, window, cx)))
            .on_action(cx.listener(|this, _: &TightenFloor, window, cx| this.adjust_floor(-2, window, cx)))
            .on_action(cx.listener(|this, _: &RelaxFloor, window, cx| this.adjust_floor(2, window, cx)))
            .on_action(cx.listener(|this, _: &ToggleAutoSync, window, cx| this.set_auto_sync(!this.snapshot.auto_sync, window, cx)))
            .on_action(cx.listener(|this, _: &DecreaseOffset, window, cx| this.adjust_offset(-2, window, cx)))
            .on_action(cx.listener(|this, _: &IncreaseOffset, window, cx| this.adjust_offset(2, window, cx)))
            .on_action(cx.listener(|this, _: &RefreshDevices, window, cx| this.run_operation(Operation::Refresh, window, cx)))
            .child(div().p_3().flex().flex_col().gap_1()
                .child(format!("Outputs · {} routed / {} available",
                    self.snapshot.endpoints.iter().filter(|endpoint| endpoint.active).count(), self.snapshot.endpoints.len()))
                .child(div().text_xs().text_color(cx.theme().muted_foreground)
                    .child("Select an output to route audio and adjust its settings")))
            .child(div().h(Size::Small.table_row_height() * (self.snapshot.endpoints.len().clamp(1, 6) + 1) as f32).min_h_0().child(DataTable::new(&self.table)
                .accessibility_label("Hardware outputs").bordered(false).small()))
            .child(div().p_3().text_xs().text_color(cx.theme().muted_foreground)
                .child("Matrix: ↑↓ select · Space route · M mute · ←→ volume · [ ] master · A sync · − + floor · , . offset · R refresh"))
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut inspector = div().flex().flex_col().gap_3().p_3();
        if let Some(endpoint) = self.selected() {
            inspector = inspector
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().text_lg().child(endpoint.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(endpoint.id.clone()),
                                ),
                        )
                        .child(
                            Button::new("device-route")
                                .outline()
                                .label(if endpoint.desired_active {
                                    "Stop routing"
                                } else {
                                    "Route audio"
                                })
                                .disabled(!self.snapshot.running)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_target(window, cx)
                                })),
                        )
                        .child(
                            Button::new("device-mute")
                                .outline()
                                .label(if endpoint.muted {
                                    "Unmute output"
                                } else {
                                    "Mute output"
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_device_mute(window, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap_4()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(format!(
                                    "Output volume {:.0}%",
                                    self.device_volume.read(cx).value().end() * 100.
                                ))
                                .child(
                                    div().id("device-volume").child(
                                        Slider::new(&self.device_volume)
                                            .accessibility_label("Selected output volume"),
                                    ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(gain_summary(
                                            self.device_volume.read(cx).value().end(),
                                            endpoint.muted,
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .w_48()
                                .flex_shrink_0()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child("Extra delay")
                                .child(
                                    div().id("offset-input").child(
                                        NumberInput::new(&self.offset)
                                            .accessibility_label(
                                                "Extra output delay in milliseconds",
                                            )
                                            .suffix("ms"),
                                    ),
                                )
                                .children(self.offset_error.as_ref().map(|error| {
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().danger)
                                        .child(error.clone())
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_3()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "Ring {:.1} ms · playback buffer {:.1} ms",
                            endpoint.ring_ms, endpoint.hardware_ms
                        ))
                        .child(format!(
                            "Estimated latency {:.1} ms · not measured DAC latency",
                            endpoint.latency_ms
                        ))
                        .child(format!(
                            "{} Hz · {} ch · {} bit",
                            endpoint.sample_rate, endpoint.channels, endpoint.bits
                        ))
                        .child(format!(
                            "Underruns {} · overruns {}",
                            endpoint.underruns, endpoint.overruns
                        )),
                );
        } else {
            inspector = inspector.child("No output selected").child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("Select an output above to change routing, volume, and delay."),
            );
        }
        inspector = inspector.child(self.telemetry.clone());
        div()
            .id("inspector")
            .test_support()
            .aria_label("Selected output controls")
            .w_full()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .id("inspector-scroll")
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(inspector),
            )
    }

    fn show_audio_details(&self, window: &mut Window, cx: &mut Context<Self>) {
        let status = SharedString::from(self.snapshot.status.clone());
        let error = self
            .snapshot
            .error
            .clone()
            .or_else(|| {
                self.operation_error
                    .as_ref()
                    .map(|error| format!("Last requested operation: {error}"))
            })
            .map(SharedString::from);
        let endpoint_errors: Vec<SharedString> = self
            .snapshot
            .endpoints
            .iter()
            .filter_map(|endpoint| {
                endpoint
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {error}", endpoint.name).into())
            })
            .collect();
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog.title("Audio details").child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(status.clone())
                    .children(error.as_ref().map(|error| {
                        div()
                            .id("audio-detail-error")
                            .test_support()
                            .aria_label(error.clone())
                            .text_color(cx.theme().danger)
                            .child(error.clone())
                    }))
                    .children(
                        endpoint_errors
                            .iter()
                            .map(|error| div().text_color(cx.theme().danger).child(error.clone())),
                    ),
            )
        });
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let loading =
            self.operation.is_some() || (!self.snapshot.capturing && !self.snapshot.startup_failed);
        div()
            .id("router-status")
            .test_support()
            .role(gpui_kit::Role::Status)
            .aria_label(self.snapshot.status.clone())
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(loading.then(|| Spinner::new().small()))
                    .child(match self.operation {
                        Some(Operation::Refresh) => "Refreshing outputs…".to_owned(),
                        Some(Operation::Retry) => "Retrying audio startup…".to_owned(),
                        None => self.snapshot.status.clone(),
                    })
                    .child(div().flex_1())
                    .children(self.snapshot.startup_failed.then(|| {
                        Button::new("retry-startup")
                            .outline()
                            .label("Retry audio startup")
                            .disabled(self.operation.is_some())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run_operation(Operation::Retry, window, cx)
                            }))
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Ctrl+Q quit"),
                    ),
            )
    }
}

impl Render for RouterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_master(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(self.render_matrix(cx))
                    .child(self.render_inspector(cx)),
            )
            .child(self.render_status(cx))
    }
}

fn gain_summary(volume: f32, muted: bool) -> String {
    let (os, dsp) = crate::dsp::split_hybrid_volume(volume);
    let effective = crate::dsp::effective_total_gain(volume);
    let gain = if muted || effective <= 0.0001 {
        "−∞ dB".to_owned()
    } else {
        format!("{:+.1} dB", 20.0 * effective.log10())
    };
    format!("OS {:.0}% · DSP {:.2}× · gain {gain}", os * 100.0, dsp)
}

fn sync_slider(
    state: &Entity<SliderState>,
    value: f32,
    window: &mut Window,
    cx: &mut Context<RouterView>,
) {
    if (state.read(cx).value().end() - value).abs() > 0.0001 {
        state.update(cx, |state, cx| state.set_value(value, window, cx));
    }
}

fn sync_number(
    state: &Entity<InputState>,
    value: String,
    window: &mut Window,
    cx: &mut Context<RouterView>,
) {
    if state.read(cx).value().as_ref() != value {
        state.update(cx, |state, cx| state.set_value(value, window, cx));
    }
}

// Meter-only ticks invalidate TelemetryView, not the routing controls and output table.
fn same_model(previous: &EngineSnapshot, next: &EngineSnapshot) -> bool {
    previous.endpoints == next.endpoints
        && previous.master_volume == next.master_volume
        && previous.master_muted == next.master_muted
        && previous.target_floor_ms == next.target_floor_ms
        && previous.auto_sync == next.auto_sync
        && previous.source_rate == next.source_rate
        && previous.capturing == next.capturing
        && previous.running == next.running
        && previous.startup_failed == next.startup_failed
        && previous.status == next.status
        && previous.error == next.error
}

fn same_table(previous: &[EndpointSnapshot], next: &[EndpointSnapshot]) -> bool {
    previous.len() == next.len()
        && previous.iter().zip(next).all(|(a, b)| {
            a.id == b.id
                && a.name == b.name
                && a.is_default == b.is_default
                && a.active == b.active
                && a.desired_active == b.desired_active
                && a.muted == b.muted
                && a.volume == b.volume
                && a.offset_ms == b.offset_ms
                && a.latency_ms == b.latency_ms
                && a.sample_rate == b.sample_rate
                && a.channels == b.channels
                && a.bits == b.bits
                && a.error == b.error
        })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

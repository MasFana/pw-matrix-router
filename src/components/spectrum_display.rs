use crate::dsp::TelemetrySnapshot;
use gpui_kit::component::ActiveTheme;
use gpui_kit::{
    App, Bounds, Context, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, canvas, div, fill, point, size,
};

/// Retained meter boundary: telemetry ticks do not rebuild routing controls or the table.
pub(crate) struct TelemetryView {
    snapshot: TelemetrySnapshot,
}

impl TelemetryView {
    pub(crate) fn new(snapshot: TelemetrySnapshot) -> Self {
        Self { snapshot }
    }

    pub(crate) fn set_snapshot(&mut self, snapshot: TelemetrySnapshot, cx: &mut Context<Self>) {
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
            cx.notify();
        }
    }
}

impl Render for TelemetryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let telemetry = self.snapshot;
        div()
            .border_t_1()
            .border_color(cx.theme().border)
            .pt_3()
            .flex()
            .items_start()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child("Virtual input · stereo levels")
                    .child(stereo_meters(telemetry, cx))
                    .child(stereo_balance(telemetry, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child("Spectrum · 16 bands and peak holds")
                    .child(spectrum(telemetry, cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "Capture {} Hz · {} ch · {} bit · {:.2} ms period",
                                telemetry.sample_rate,
                                telemetry.channels,
                                telemetry.bits,
                                telemetry.period_ms
                            )),
                    ),
            )
    }
}

fn level(value: f64) -> f32 {
    if !value.is_finite() || value <= 0. {
        0.
    } else {
        ((20. * value.log10() + 60.) / 60.).clamp(0., 1.) as f32
    }
}

// Pixel coordinates here are resolved canvas geometry, not fixed product sizing.
fn rectangle(
    window: &mut Window,
    x: Pixels,
    y: Pixels,
    width: Pixels,
    height: Pixels,
    color: Hsla,
) {
    window.paint_quad(fill(Bounds::new(point(x, y), size(width, height)), color));
}

fn stereo_meters(telemetry: TelemetrySnapshot, cx: &App) -> impl IntoElement {
    let background = cx.theme().muted;
    let rms_color = cx.theme().success;
    let peak_color = cx.theme().warning;
    let hold_color = cx.theme().foreground;
    div()
        .id("stereo-levels")
        .test_support()
        .aria_label("Stereo levels")
        .flex()
        .flex_col()
        .gap_2()
        .children(
            [
                (
                    "L",
                    telemetry.rms_l,
                    telemetry.peak_l,
                    telemetry.peak_hold_l,
                ),
                (
                    "R",
                    telemetry.rms_r,
                    telemetry.peak_r,
                    telemetry.peak_hold_r,
                ),
            ]
            .into_iter()
            .map(move |(channel, rms, peak, hold)| {
                let rms_level = level(rms);
                let peak_level = level(peak);
                let hold_level = level(hold);
                div()
                    .id(if channel == "L" {
                        "left-level"
                    } else {
                        "right-level"
                    })
                    .test_support()
                    .role(gpui_kit::Role::Meter)
                    .aria_label(if channel == "L" {
                        "Left audio level"
                    } else {
                        "Right audio level"
                    })
                    .aria_numeric_value(rms_level as f64 * 60. - 60.)
                    .aria_min_numeric_value(-60.)
                    .aria_max_numeric_value(0.)
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w_4().flex_shrink_0().child(channel))
                    .child(
                        canvas(
                            |bounds, _, _| bounds,
                            move |_, bounds, window, _| {
                                let width = bounds.size.width;
                                let height = bounds.size.height;
                                rectangle(
                                    window,
                                    bounds.left(),
                                    bounds.top(),
                                    width,
                                    height,
                                    background,
                                );
                                rectangle(
                                    window,
                                    bounds.left(),
                                    bounds.top(),
                                    width * peak_level,
                                    height,
                                    peak_color.opacity(0.45),
                                );
                                rectangle(
                                    window,
                                    bounds.left(),
                                    bounds.top() + height * 0.25,
                                    width * rms_level,
                                    height * 0.5,
                                    rms_color,
                                );
                                let marker = height * 0.12;
                                rectangle(
                                    window,
                                    bounds.left() + (width - marker) * hold_level,
                                    bounds.top(),
                                    marker,
                                    height,
                                    hold_color,
                                );
                            },
                        )
                        .flex_1()
                        .min_w_0()
                        .h_3(),
                    )
            }),
        )
        .child(
            div()
                .flex()
                .pl_6()
                .justify_between()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .children(["−60", "−40", "−20", "0 dBFS"]),
        )
}

fn stereo_balance(telemetry: TelemetrySnapshot, cx: &App) -> impl IntoElement {
    let background = cx.theme().muted;
    let marker_color = cx.theme().foreground;
    let signal = cx.theme().primary;
    let value = if telemetry.balance.is_finite() {
        telemetry.balance.clamp(-1., 1.) as f32
    } else {
        0.
    };
    div()
        .id("stereo-balance")
        .test_support()
        .role(gpui_kit::Role::Meter)
        .aria_label("Balance")
        .aria_numeric_value(value as f64)
        .aria_min_numeric_value(-1.)
        .aria_max_numeric_value(1.)
        .flex()
        .items_start()
        .gap_2()
        .child(div().w_24().flex_shrink_0().text_xs().child("Balance"))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    canvas(
                        |bounds, _, _| bounds,
                        move |_, bounds, window, _| {
                            let width = bounds.size.width;
                            let height = bounds.size.height;
                            let center = bounds.left() + width * 0.5;
                            let fill_width = width * value.abs() * 0.5;
                            let marker = height * 0.2;
                            rectangle(
                                window,
                                bounds.left(),
                                bounds.top(),
                                width,
                                height,
                                background,
                            );
                            rectangle(
                                window,
                                if value < 0. {
                                    center - fill_width
                                } else {
                                    center
                                },
                                bounds.top(),
                                fill_width,
                                height,
                                signal,
                            );
                            rectangle(
                                window,
                                center - marker * 0.5,
                                bounds.top(),
                                marker,
                                height,
                                marker_color.opacity(0.4),
                            );
                            rectangle(
                                window,
                                bounds.left() + (width - marker) * (value + 1.) * 0.5,
                                bounds.top(),
                                marker,
                                height,
                                marker_color,
                            );
                        },
                    )
                    .w_full()
                    .h_2(),
                )
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Left")
                        .child("Right"),
                ),
        )
}

fn spectrum(telemetry: TelemetrySnapshot, cx: &App) -> impl IntoElement {
    let background = cx.theme().muted;
    let signal = cx.theme().primary;
    let hold = cx.theme().foreground;
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            canvas(
                |bounds, _, _| bounds,
                move |_, bounds, window, _| {
                    let band_width = bounds.size.width / 16.;
                    let gutter = band_width * 0.15;
                    let marker = bounds.size.height * 0.015;
                    for ix in 0..16 {
                        let x = bounds.left() + band_width * ix as f32;
                        let height =
                            bounds.size.height * telemetry.spectrum[ix].clamp(0., 1.) as f32;
                        rectangle(
                            window,
                            x,
                            bounds.top(),
                            band_width - gutter,
                            bounds.size.height,
                            background,
                        );
                        rectangle(
                            window,
                            x,
                            bounds.bottom() - height,
                            band_width - gutter,
                            height,
                            signal,
                        );
                        let hold_y = bounds.bottom()
                            - (bounds.size.height - marker)
                                * telemetry.spec_hold[ix].clamp(0., 1.) as f32
                            - marker;
                        rectangle(window, x, hold_y, band_width - gutter, marker, hold);
                    }
                },
            )
            .w_full()
            .h_24(),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .children(["45 Hz", "200 Hz", "1 kHz", "5 kHz", "13 kHz"]),
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn meter_scale_handles_silence_and_clipping() {
        assert_eq!(super::level(0.), 0.);
        assert_eq!(super::level(f64::NAN), 0.);
        assert_eq!(super::level(0.001), 0.);
        assert!((super::level(0.1) - 2. / 3.).abs() < 0.0001);
        assert_eq!(super::level(2.), 1.);
    }
}

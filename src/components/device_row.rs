use crate::audio::EndpointSnapshot;
use gpui_kit::component::{
    ActiveTheme,
    table::{Column, TableDelegate, TableState},
};
use gpui_kit::{
    App, Context, Div, InteractiveElement, IntoElement, ParentElement, Pixels, SharedString,
    Stateful, Styled, Window, div,
};

struct OutputRow {
    id: SharedString,
    cells: [SharedString; 6],
}

/// Prepared presentation only: cell rendering never reads the engine or its owner.
pub(crate) struct OutputTable {
    rows: Vec<OutputRow>,
    columns: Vec<Column>,
    pub(crate) loading: bool,
    pub(crate) empty_message: SharedString,
}

impl OutputTable {
    pub(crate) fn new(rem_size: Pixels) -> Self {
        let columns = [
            ("name", "Output", 20.),
            ("route", "Route", 7.),
            ("volume", "Volume", 7.),
            ("latency", "Est. latency", 8.),
            ("offset", "Offset", 7.),
            ("format", "Format", 15.),
        ]
        .into_iter()
        .map(|(key, label, width)| {
            Column::new(key, label)
                .width(rem_size * width)
                .min_width(rem_size * 4.)
                .movable(false)
        })
        .collect();
        Self {
            rows: Vec::new(),
            columns,
            loading: false,
            empty_message: "No hardware outputs found. Connect an output and refresh.".into(),
        }
    }

    /// Reuse row storage and unchanged cells from the previous published snapshot.
    pub(crate) fn replace(
        &mut self,
        previous: &[EndpointSnapshot],
        endpoints: &[EndpointSnapshot],
    ) {
        self.rows.resize_with(endpoints.len(), || OutputRow {
            id: "".into(),
            cells: std::array::from_fn(|_| "".into()),
        });
        for (ix, (row, endpoint)) in self.rows.iter_mut().zip(endpoints).enumerate() {
            let old = previous.get(ix).filter(|old| old.id == endpoint.id);
            if old.is_none() {
                row.id = endpoint.id.clone().into();
            }
            if old.is_none_or(|old| {
                old.name != endpoint.name || old.is_default != endpoint.is_default
            }) {
                row.cells[0] = format!(
                    "{}{}",
                    endpoint.name,
                    if endpoint.is_default {
                        " · default"
                    } else {
                        ""
                    }
                )
                .into();
            }
            if old.is_none_or(|old| {
                old.error.is_some() != endpoint.error.is_some()
                    || old.active != endpoint.active
                    || old.desired_active != endpoint.desired_active
            }) {
                row.cells[1] = if endpoint.error.is_some() {
                    "Failed"
                } else if endpoint.active {
                    "Routed"
                } else if endpoint.desired_active {
                    "Pending"
                } else {
                    "Off"
                }
                .into();
            }
            if old.is_none_or(|old| old.muted != endpoint.muted || old.volume != endpoint.volume) {
                row.cells[2] = if endpoint.muted {
                    "Muted".into()
                } else {
                    format!("{:.0}%", endpoint.volume * 100.).into()
                };
            }
            if old.is_none_or(|old| old.latency_ms != endpoint.latency_ms) {
                row.cells[3] = format!("{:.1} ms", endpoint.latency_ms).into();
            }
            if old.is_none_or(|old| old.offset_ms != endpoint.offset_ms) {
                row.cells[4] = format!("+{} ms", endpoint.offset_ms).into();
            }
            if old.is_none_or(|old| {
                old.sample_rate != endpoint.sample_rate
                    || old.channels != endpoint.channels
                    || old.bits != endpoint.bits
            }) {
                row.cells[5] = format!(
                    "{} Hz · {} ch · {} bit",
                    endpoint.sample_rate, endpoint.channels, endpoint.bits
                )
                .into();
            }
        }
    }

    pub(crate) fn id_at(&self, ix: usize) -> Option<&str> {
        self.rows.get(ix).map(|row| row.id.as_ref())
    }
}

impl TableDelegate for OutputTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }
    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }
    fn column(&self, ix: usize, _: &App) -> Column {
        self.columns[ix].clone()
    }
    fn loading(&self, _: &App) -> bool {
        self.loading && self.rows.is_empty()
    }
    fn render_tr(
        &mut self,
        ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        div().id(self.rows[ix].id.clone())
    }
    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .overflow_hidden()
            .child(self.rows[row].cells[col].clone())
    }
    fn cell_label(&self, row: usize, col: usize, _: &App) -> Option<SharedString> {
        self.rows.get(row).map(|row| row.cells[col].clone())
    }
    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .p_6()
            .text_color(cx.theme().muted_foreground)
            .child(self.empty_message.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::OutputTable;
    use crate::audio::EndpointSnapshot;
    use gpui_kit::px;

    #[test]
    fn latency_updates_reuse_unaffected_row_cells() {
        let mut endpoint = EndpointSnapshot::default();
        endpoint.id = "output-a".into();
        endpoint.name = "Hardware output".into();
        endpoint.latency_ms = 12.;
        let mut table = OutputTable::new(px(16.));
        table.replace(&[], std::slice::from_ref(&endpoint));
        let rows = table.rows.as_ptr();
        let id = table.rows[0].id.as_ptr();
        let cells = table.rows[0].cells.each_ref().map(|cell| cell.as_ptr());
        let mut next = endpoint.clone();
        next.latency_ms = 13.5;
        table.replace(&[endpoint], &[next]);
        assert_eq!(table.rows.as_ptr(), rows);
        assert_eq!(table.rows[0].id.as_ptr(), id);
        for ix in [0, 1, 2, 4, 5] {
            assert_eq!(table.rows[0].cells[ix].as_ptr(), cells[ix]);
        }
        assert_eq!(table.rows[0].cells[3].as_ref(), "13.5 ms");
    }

    #[test]
    fn row_replacement_handles_reordering_and_disconnect() {
        let mut a = EndpointSnapshot::default();
        a.id = "a".into();
        a.name = "Output A".into();
        let mut b = a.clone();
        b.id = "b".into();
        b.name = "Output B".into();
        b.muted = true;
        let mut table = OutputTable::new(px(16.));
        table.replace(&[], &[a.clone(), b.clone()]);
        table.replace(&[a.clone(), b.clone()], &[b.clone(), a.clone()]);
        assert_eq!(table.id_at(0), Some("b"));
        assert_eq!(table.rows[0].cells[0].as_ref(), "Output B");
        assert_eq!(table.rows[0].cells[2].as_ref(), "Muted");
        assert_eq!(table.id_at(1), Some("a"));
        table.replace(&[b.clone(), a], std::slice::from_ref(&b));
        assert_eq!(table.rows.len(), 1);
        table.replace(&[], &[]);
        assert!(table.rows.is_empty());
    }
}

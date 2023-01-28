use std::collections::VecDeque;

use chrono::{DateTime, Duration, Utc};
use iced::pure::{row, text};
use iced::Size;
use iced::{pure::Element, Length};
use log::info;
use plotters_iced::pure::{Chart, ChartWidget};
use plotters_iced::{ChartBuilder, DrawingBackend};

use iced::pure::widget::canvas::{Cache, Frame, Geometry};

use super::Message;

#[derive(Debug)]
pub(crate) struct WaveformChart {
    pub(crate) cache: Cache,
    pub(crate) values: VecDeque<(DateTime<Utc>, f32)>,
}

impl Chart<Message> for WaveformChart {
    type State = ();

    #[inline]
    fn draw<F: Fn(&mut Frame)>(&self, bounds: Size, draw_fn: F) -> Geometry {
        self.cache.draw(bounds, draw_fn)
    }

    fn build_chart<DB: DrawingBackend>(&self, state: &Self::State, mut builder: ChartBuilder<DB>) {
        use plotters::prelude::*;

        // Acquire time range
        let newest_time = self.values.back().map_or(Utc::now(), |x| x.0);
        let oldest_time = self.values.front().map_or(Utc::now(), |x| x.0);
        // let oldest_time = Utc::now() - Duration::seconds(5);

        let mut chart = builder
            .margin(30)
            .x_label_area_size(30)
            .y_label_area_size(30)
            .build_cartesian_2d(oldest_time..newest_time, 0.0_f32..1.0)
            .unwrap();

        chart
            .configure_mesh()
            .x_labels(3)
            .y_labels(3)
            // .y_label_style(
            //     ("sans-serif", 15)
            //         .into_font()
            //         .color(&plotters::style::colors::BLACK.mix(0.8))
            //         .transform(FontTransform::RotateAngle(30.0)),
            // )
            .draw()
            .unwrap();

        chart
            .draw_series(LineSeries::new(
                self.values.iter().map(|x| (x.0, x.1)),
                &RED,
            ))
            .unwrap();

        // chart
        // .draw_series(PointSeries::of_element(
        //     self.values.iter().map(|x| (x.0, x.1)),
        //     1.0,
        //     &RED,
        //     &|coord, size, style| EmptyElement::at(coord) + Circle::new((0, 0), size, style),
        // ))
        // .unwrap();
    }
}

pub(crate) struct UpdateState {
    /// Tim eof new value to add
    pub time: DateTime<Utc>,
    pub new_value: f32,
    /// Values that are older than old_time will get removed
    pub old_time: DateTime<Utc>,
}

impl WaveformChart {
    pub(crate) fn new() -> Self {
        Self {
            cache: Cache::new(),
            values: VecDeque::new(),
        }
    }

    pub(crate) fn update(&mut self, state: UpdateState) {
        self.values.push_back((state.time, state.new_value));
        self.values.retain(|x| x.0 > state.old_time);
        self.cache.clear();
    }

    pub(crate) fn view(&self) -> Element<Message> {
        let mut row = row();

        row = row.push(
            ChartWidget::new(self)
                .height(Length::Units(200))
                .width(Length::Fill),
        );
        row = row.push(text(format!(
            "{:0.2}",
            self.values.back().map_or(0.0, |x| x.1)
        )));

        row.into()
    }
}

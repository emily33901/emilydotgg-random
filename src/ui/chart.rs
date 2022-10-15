use std::collections::VecDeque;
use std::time::Instant;

use chrono::{DateTime, Utc};
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
    pub(crate) values: VecDeque<(DateTime<Utc>, i32)>,
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

        let mut chart = builder
            .margin(30)
            .x_label_area_size(30)
            .y_label_area_size(30)
            .build_cartesian_2d(oldest_time..newest_time, 0..0x1000)
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
            .draw_series(AreaSeries::new(
                self.values.iter().map(|x| (x.0, x.1 as i32)),
                0,
                &RED,
            ))
            .unwrap();
    }
}

pub(crate) struct UpdateState {
    pub new_value: i32,
}

impl WaveformChart {
    pub(crate) fn new() -> Self {
        Self {
            cache: Cache::new(),
            values: VecDeque::new(),
        }
    }

    pub(crate) fn update(&mut self, state: UpdateState) {
        self.values.push_back((Utc::now(), state.new_value));
        if self.values.len() > 50 {
            self.values.pop_front();
        }
        self.cache.clear();
    }

    pub(crate) fn view(&self) -> Element<Message> {
        let chart = ChartWidget::new(self)
            .height(Length::Units(300))
            .width(Length::Fill);

        chart.into()
    }
}

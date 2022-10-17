use std::ffi::c_void;
use std::sync::Arc;

use fpsdk::host::Host;
use futures::lock::Mutex;
use futures::stream;
use iced::pure::widget::Text;
use iced::pure::{button, column, text, Application};
use iced::pure::{scrollable, Element};
use iced::{Alignment, Command, Settings};
use log::info;
use tokio::sync::{mpsc, watch};

use crate::ui::chart::UpdateState;
use crate::CONTROLLER_COUNT;

use self::chart::WaveformChart;

mod chart;

#[derive(Debug)]
pub struct App {
    ui_message_rx: Arc<Mutex<mpsc::Receiver<UIMessage>>>,
    host_message_tx: Arc<Mutex<mpsc::Sender<HostMessage>>>,
    hwnd: parking_lot::Mutex<Option<*mut c_void>>,
    charts: Vec<chart::WaveformChart>,
    should_exit: bool,
    should_draw: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    None,
    HostMessage(UIMessage),
}

/// Message from the Plugin to the UI
#[derive(Debug, Clone)]
pub enum UIMessage {
    ShowEditor(Option<*mut c_void>),
    UpdateControllers(Vec<(usize, u64)>),
    Die,
}

unsafe impl Send for UIMessage {}
unsafe impl Sync for UIMessage {}

#[derive(Debug, Clone, Copy)]
/// Message from the UI to the Plugin
pub enum HostMessage {
    SetEditorHandle(Option<*mut c_void>),
}

unsafe impl Send for HostMessage {}
unsafe impl Sync for HostMessage {}

pub struct AppFlags {
    ui_message_rx: mpsc::Receiver<UIMessage>,
    host_message_tx: mpsc::Sender<HostMessage>,
}

impl Application for App {
    type Message = Message;

    fn new(flags: Self::Flags) -> (Self, Command<Self::Message>) {
        (
            Self {
                ui_message_rx: Arc::new(Mutex::new(flags.ui_message_rx)),
                host_message_tx: Arc::new(Mutex::new(flags.host_message_tx)),
                hwnd: parking_lot::Mutex::new(None),
                charts: (0..CONTROLLER_COUNT)
                    .map(|_| WaveformChart::new())
                    .collect(),
                should_exit: false,
                should_draw: false,
            },
            Command::none(),
        )
    }

    fn title(&self) -> String {
        String::from("test Counter - Iced")
    }

    fn update(&mut self, message: Message) -> iced::Command<Self::Message> {
        let command = match message {
            Message::None => None,
            Message::HostMessage(UIMessage::ShowEditor(hwnd)) => {
                self.should_draw = hwnd.is_some();

                unsafe {
                    use windows::Win32::Foundation::*;
                    use windows::Win32::UI::WindowsAndMessaging;
                    let self_hwnd = self.hwnd.lock();
                    let self_hwnd = HWND(self_hwnd.map_or(0, |x| x as isize) as isize);
                    if let Some(parent_hwnd) = hwnd.map(|x| HWND(x as isize)) {
                        WindowsAndMessaging::SetParent(self_hwnd, parent_hwnd);
                        WindowsAndMessaging::ShowWindow(self_hwnd, WindowsAndMessaging::SW_SHOW);
                    } else {
                        WindowsAndMessaging::SetParent(self_hwnd, HWND(0));
                        WindowsAndMessaging::ShowWindow(self_hwnd, WindowsAndMessaging::SW_HIDE);
                    }
                }

                info!("Set parent!");
                let message = HostMessage::SetEditorHandle(self.hwnd.lock().clone());
                let host_message_tx = self.host_message_tx.clone();

                Some(iced::Command::perform(
                    async move {
                        host_message_tx.lock().await.send(message).await.unwrap();
                    },
                    |_| Message::None,
                ))
            }
            Message::HostMessage(UIMessage::UpdateControllers(updates)) => {
                for (controller_i, new_v) in updates {
                    self.charts[controller_i].update(UpdateState {
                        new_value: (new_v >> 32) as i32,
                    });
                }
                None
            }
            Message::HostMessage(UIMessage::Die) => {
                info!("UI going down!");
                self.should_exit = true;
                None
            }
            Message::HostMessage(message) => {
                info!("UI got message {message:?}");
                None
            }
        };

        command.unwrap_or(iced::Command::none())
    }

    fn subscription(&self) -> iced::Subscription<Self::Message> {
        iced_native::subscription::Subscription::batch([iced_native::Subscription::from_recipe(
            UIMessageWatcher {
                rx: self.ui_message_rx.clone(),
            },
        )])
    }

    fn view(&self) -> Element<Message> {
        // TODO(emily): Probably shouldn't just spin here, but in that case we would need a better way to resume
        // that isnt a UIMessage. A Convar would probably work well?
        if !self.should_draw {
            return column().into();
        }

        let mut column = column().padding(20).align_items(Alignment::Center);

        for chart in &self.charts {
            column = column.push(chart.view());
        }

        scrollable(column).into()
    }

    fn should_exit(&self) -> bool {
        self.should_exit
    }

    type Executor = iced::executor::Default;

    type Flags = AppFlags;

    fn hwnd(&self, hwnd: *mut std::ffi::c_void) {
        *self.hwnd.lock() = Some(hwnd);
    }
}

impl App {}

#[derive(Clone)]
struct UIMessageWatcher {
    rx: Arc<Mutex<mpsc::Receiver<UIMessage>>>,
}

impl<H, Event> iced_native::subscription::Recipe<H, Event> for UIMessageWatcher
where
    H: std::hash::Hasher,
{
    type Output = Message;

    fn hash(&self, state: &mut H) {
        use std::hash::Hash;

        std::any::TypeId::of::<Self>().hash(state);
        0.hash(state);
    }

    fn stream(
        self: Box<Self>,
        _input: stream::BoxStream<Event>,
    ) -> stream::BoxStream<Self::Output> {
        Box::pin(futures::stream::unfold(self, |mut state| async move {
            state.rx.lock().await.recv().await.map_or(None, |message| {
                Some((Message::HostMessage(message), state.clone()))
            })
        }))
    }
}

pub(crate) fn run(
    ui_message_rx: tokio::sync::mpsc::Receiver<UIMessage>,
    host_message_tx: mpsc::Sender<HostMessage>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| {
        info!("Starting UI");
        let mut settings = iced::Settings::with_flags(AppFlags {
            ui_message_rx,
            host_message_tx,
        });
        settings.antialiasing = true;
        App::run(settings).unwrap();
        info!("ui thread finished");
    })
}

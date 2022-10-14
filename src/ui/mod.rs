use std::ffi::c_void;
use std::sync::Arc;

use fpsdk::host::Host;
use futures::lock::Mutex;
use futures::stream;
use iced::pure::widget::Text;
use iced::pure::Element;
use iced::pure::{button, column, text, Application};
use iced::{Alignment, Command, Settings};
use log::info;
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
pub struct App {
    value: i32,
    ui_message_rx: Arc<Mutex<mpsc::Receiver<UIMessage>>>,
    host_message_tx: Arc<Mutex<mpsc::Sender<HostMessage>>>,
    hwnd: parking_lot::Mutex<Option<*mut c_void>>,
}

#[derive(Debug, Clone, Copy)]
pub enum Message {
    None,
    IncrementPressed,
    DecrementPressed,
    HostMessage(UIMessage),
}

#[derive(Debug, Clone, Copy)]
pub enum UIMessage {
    ShowEditor(Option<*mut c_void>),
}

unsafe impl Send for UIMessage {}
unsafe impl Sync for UIMessage {}

#[derive(Debug, Clone, Copy)]
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
                value: 0,
                ui_message_rx: Arc::new(Mutex::new(flags.ui_message_rx)),
                host_message_tx: Arc::new(Mutex::new(flags.host_message_tx)),
                hwnd: parking_lot::Mutex::new(None),
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
            Message::IncrementPressed => {
                self.value += 1;
                None
            }
            Message::DecrementPressed => {
                self.value -= 1;
                None
            }
            Message::HostMessage(UIMessage::ShowEditor(hwnd)) => {
                unsafe {
                    use windows::Win32::Foundation::*;
                    use windows::Win32::UI::WindowsAndMessaging;
                    let self_hwnd = self.hwnd.lock();
                    let self_hwnd = HWND(self_hwnd.map_or(0, |x| x as isize) as isize);
                    let parent_hwnd = HWND(hwnd.map_or(0, |x| x as isize));

                    WindowsAndMessaging::SetParent(self_hwnd, parent_hwnd);
                    WindowsAndMessaging::ShowWindow(self_hwnd, WindowsAndMessaging::SW_SHOW);
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
        column()
            .padding(20)
            .align_items(Alignment::Center)
            .push(button(Text::new("Increment")).on_press(Message::IncrementPressed))
            .push(Text::new(self.value.to_string()).size(50))
            .push(button(Text::new("Decrement")).on_press(Message::DecrementPressed))
            .into()
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

pub(crate) fn run() -> (
    tokio::sync::mpsc::Sender<UIMessage>,
    mpsc::Receiver<HostMessage>,
) {
    let (ui_message_tx, ui_message_rx) = tokio::sync::mpsc::channel::<UIMessage>(10);
    let (host_message_tx, host_message_rx) = tokio::sync::mpsc::channel::<HostMessage>(10);

    std::thread::spawn(|| {
        info!("Starting UI");
        App::run(iced::Settings::with_flags(AppFlags {
            ui_message_rx,
            host_message_tx,
        }))
        .unwrap();
        info!("ui thread finished");
    });

    (ui_message_tx, host_message_rx)
}

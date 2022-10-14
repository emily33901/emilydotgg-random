use parking_lot::Mutex;
use std::io::{self, Read};
use std::panic::RefUnwindSafe;
use std::sync::{Arc, Once};

use bincode;
use log::{error, info, trace, LevelFilter};
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use simple_logging;

use fpsdk::host::{self, Event, GetName, Host};
use fpsdk::plugin::{self, Info, InfoBuilder, Plugin, StateReader, StateWriter};
use fpsdk::plugin::{message, PluginProxy};
use fpsdk::{create_plugin, AsRawPtr, MidiMessage, ProcessParamFlags, TimeFormat, ValuePtr};

static ONCE: Once = Once::new();
const LOG_PATH: &str = "S:/emilydotgg-random.log";

mod ui;

#[derive(Debug)]
struct Simple {
    host: Host,
    tag: plugin::Tag,
    state: State,
    ui_send: Option<Mutex<tokio::sync::mpsc::Sender<ui::UIMessage>>>,
    host_receive: Option<Mutex<tokio::sync::mpsc::Receiver<ui::HostMessage>>>,
    proxy: Option<PluginProxy>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct State {
    seed: u64,
    speed: u64,
    tick: u64,
    test_val: u64,
}

impl Plugin for Simple {
    fn new(host: Host, tag: plugin::Tag) -> Self {
        init_log();

        info!("init plugin with tag {}", tag);

        Self {
            host,
            tag,
            state: Default::default(),
            ui_send: None,
            host_receive: None,
            proxy: None,
        }
    }

    fn proxy(&mut self, handle: plugin::PluginProxy) {
        self.proxy = Some(handle);
    }

    fn info(&self) -> Info {
        info!("plugin {} will return info", self.tag);

        InfoBuilder::new_effect("emilydotgg-random", "Random", 3)
            .want_new_tick()
            .with_out_ctrls(100)
            .no_process()
            .build()
    }

    fn save_state(&mut self, writer: StateWriter) {
        match bincode::serialize_into(writer, &self.state) {
            Ok(_) => info!("state {:?} saved", self.state),
            Err(e) => error!("error serializing state {}", e),
        }
    }

    fn load_state(&mut self, mut reader: StateReader) {
        let mut buf = [0; std::mem::size_of::<State>()];
        reader
            .read(&mut buf)
            .and_then(|_| {
                bincode::deserialize::<State>(&buf).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        format!("error deserializing value {}", e),
                    )
                })
            })
            .and_then(|value| {
                self.state = value;
                Ok(info!("read state {:?}", self.state))
            })
            .unwrap_or_else(|e| error!("error reading value from state {}", e));
    }

    fn on_message(&mut self, message: host::Message) -> Box<dyn AsRawPtr> {
        match message {
            host::Message::SetEnabled(enabled) => {
                self.on_set_enabled(enabled, message);
            }
            host::Message::ShowEditor(handle) => self
                .ui_send
                .as_ref()
                .unwrap()
                .lock()
                .blocking_send(ui::UIMessage::ShowEditor(handle))
                .unwrap(),
            _ => (),
        }

        // See if we have any responses from the ui
        while let Ok(response) = self.host_receive.as_ref().map_or(
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected),
            |r| r.lock().try_recv(),
        ) {
            info!("Host got {response:?}");
            match response {
                ui::HostMessage::SetEditorHandle(hwnd) => {
                    self.proxy.as_ref().unwrap().set_editor_hwnd(hwnd.unwrap());
                }
            }
        }

        Box::new(0)
    }

    fn name_of(&self, message: GetName) -> String {
        info!("{} host asks name of {:?}", self.tag, message);

        match message {
            GetName::OutCtrl(index) => format!("Control {index}"),
            _ => "What?".into(),
        }
    }

    fn process_event(&mut self, event: Event) {
        info!("{} host sends event {:?}", self.tag, event);
    }

    fn tick(&mut self) {
        self.state.tick += 1;

        if self.state.speed == 0 {
            self.state.speed = 1
        }

        if self.state.tick % self.state.speed == 0 {
            let mut rng = thread_rng();

            for i in 0..100 {
                let random_value = rng.gen_range(0..0x1000);
                let random_value = random_value << 32;
                self.host.on_controller(self.tag, i, random_value);
            }
        }
    }

    fn idle(&mut self) {
        trace!("{} idle", self.tag);
    }

    // looks like it doesn't work in SDK
    fn loop_in(&mut self, message: ValuePtr) {
        trace!("{} loop_in", message.get::<String>());
    }

    fn process_param(
        &mut self,
        index: usize,
        value: ValuePtr,
        flags: ProcessParamFlags,
    ) -> Box<dyn AsRawPtr> {
        info!(
            "{} process param: index {}, value {}, flags {:?}",
            self.tag,
            index,
            value.get::<f32>(),
            flags
        );

        if flags.contains(ProcessParamFlags::FROM_MIDI | ProcessParamFlags::UPDATE_VALUE) {
            // will work if assigned to itself

            // Scale speed into a more appropriate range
            // it will be 0 - 65535 coming in and we want it to be less

            let speed = value.get::<u16>() as f64;
            let speed = (speed * 200.0) / 65535.0;

            self.state.speed = speed as u64;
        }

        Box::new(0)
    }

    fn midi_in(&mut self, message: MidiMessage) {
        trace!("receive MIDI message {:?}", message);
    }
}

impl RefUnwindSafe for Simple {}

impl Simple {
    fn on_set_enabled(&mut self, enabled: bool, message: host::Message) {
        self.log_selection();
        self.say_hello_hint();

        if enabled {
            // Enable GUI
            info!("Starting UI");
            let (ui_tx, host_rx) = ui::run();
            self.ui_send = Some(Mutex::new(ui_tx));
            self.host_receive = Some(Mutex::new(host_rx))
        }
    }

    fn log_selection(&mut self) {
        let selection = self
            .host
            .on_message(self.tag, message::GetSelTime(TimeFormat::Beats));
        self.host.on_message(
            self.tag,
            message::DebugLogMsg(format!(
                "current selection or full song range is: {:?}",
                selection
            )),
        );
    }

    fn say_hello_hint(&mut self) {
        self.host.on_hint(self.tag, "^c Hello".to_string());
    }
}

fn init_log() {
    ONCE.call_once(|| {
        _init_log();
        panic::set_hook(Box::new(|info| {
            error!("Panicked! {:?}", info);
            println!("Custom panic hook");
        }));
        info!("init log");
    });
}

#[cfg(windows)]
fn _init_log() {
    simple_logging::log_to_file(LOG_PATH, LevelFilter::Info).unwrap();
}

create_plugin!(Simple);

use std::panic;

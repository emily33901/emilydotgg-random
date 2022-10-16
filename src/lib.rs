use parking_lot::{Condvar, Mutex};
use std::io::{self, Read};
use std::panic::RefUnwindSafe;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use tokio::sync::mpsc;

use bincode;
use log::{error, info, trace, warn, LevelFilter};
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

pub(crate) const CONTROLLER_COUNT: u32 = 100;

#[derive(Debug)]
struct Simple {
    host: Host,
    tag: plugin::Tag,
    state: Arc<Mutex<State>>,
    ui_send: mpsc::Sender<ui::UIMessage>,
    ui_receive: Option<mpsc::Receiver<ui::UIMessage>>,
    host_send: mpsc::Sender<ui::HostMessage>,
    host_receive: mpsc::Receiver<ui::HostMessage>,
    proxy: Option<PluginProxy>,
    controller_values: Arc<Mutex<Option<Vec<u64>>>>,
    tick_happened: Arc<(Mutex<bool>, Condvar)>,
    ui_thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
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

        let controller_values = Arc::new(Mutex::new(Option::default()));
        let state = Arc::new(Mutex::new(State {
            speed: 400,
            tick: 1,
            ..Default::default()
        }));

        let weak_state = Arc::downgrade(&state);

        let gen_controller_values = controller_values.clone();

        let (ui_send, ui_receive) = mpsc::channel(100);
        let (host_send, host_receive) = mpsc::channel(100);

        let tick_happened = Arc::new((Mutex::new(false), Condvar::new()));
        let gen_tick_happened = tick_happened.clone();

        let gen_ui_send = ui_send.clone();

        let gen_thread_handle = std::thread::spawn(move || {
            let controller_values = gen_controller_values;

            while let Some(state) = weak_state.upgrade() {
                // Wait for a tick to happen before doing anything
                {
                    let &(ref tick_lock, ref cvar) = &*gen_tick_happened;
                    let mut tick = tick_lock.lock();
                    while !*tick {
                        cvar.wait(&mut tick);
                    }
                    *tick = false;
                }

                // Steal state
                let mut state = {
                    let state = state.lock();
                    state.clone()
                };

                if state.speed == 0 {
                    state.speed = 1;
                }

                if state.tick % state.speed == 0 {
                    let mut values = vec![0_u64; CONTROLLER_COUNT as usize];

                    if state.speed == 0 {
                        state.speed = 1;
                    }

                    // Generate some new values NOW
                    let mut rng = thread_rng();

                    for v in &mut values {
                        let random_value = rng.gen_range(0..0x1000);
                        *v = random_value << 32;
                    }

                    // Store values
                    (*controller_values.lock()).replace(values.clone());

                    let ui_message =
                        UIMessage::UpdateControllers(values.into_iter().enumerate().collect());

                    // Inform UI of what we are doing
                    gen_ui_send.blocking_send(ui_message).unwrap();
                }
            }

            info!("Gen thread done");
        });

        Self {
            host,
            tag,
            state,
            ui_send,
            ui_receive: Some(ui_receive),
            host_send,
            host_receive,
            proxy: None,
            controller_values,
            tick_happened,
            ui_thread: None,
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
        let state = self.state.lock();
        match bincode::serialize_into(writer, &*state) {
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
                self.state = Arc::new(Mutex::new(value));
                Ok(info!("read state {:?}", self.state))
            })
            .unwrap_or_else(|e| error!("error reading value from state {}", e));
    }

    fn on_message(&mut self, message: host::Message) -> Box<dyn AsRawPtr> {
        info!("Plugin got {:?}", message);
        match message {
            host::Message::SetEnabled(enabled) => {
                self.on_set_enabled(enabled, message);
            }
            host::Message::ShowEditor(handle) => self
                .ui_send
                .blocking_send(ui::UIMessage::ShowEditor(handle))
                .unwrap(),
            _ => (),
        }

        // See if we have any responses from the ui
        while let Ok(response) = self.host_receive.try_recv() {
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
        // inform gen about the tick
        {
            let &(ref lock, ref cvar) = &*self.tick_happened;
            let mut tick = lock.lock();
            *tick = true;
            cvar.notify_one();
        }

        let mut state = self.state.lock();
        state.tick += 1;

        // Inform FL about all our values
        self.controller_values.lock().take().map(|v| {
            v.into_iter()
                .enumerate()
                .map(|(i, v)| self.host.on_controller(self.tag, i, v))
        });
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

            self.state.lock().speed = speed as u64;
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
            self.ui_thread = Some(ui::run(
                self.ui_receive.take().unwrap(),
                self.host_send.clone(),
            ));
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

impl Drop for Simple {
    fn drop(&mut self) {
        info!("Dropping Plugin");
        // Tell the UI that we are dying
        self.ui_send.blocking_send(UIMessage::Die).unwrap();
        // Wait for UI to die
        if let Some(ui_thread) = self.ui_thread.take() {
            info!("Waiting for UI to die");
            ui_thread.join().unwrap();
            info!("UI died");
        } else {
            warn!("No UI?");
        }
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

use crate::ui::UIMessage;

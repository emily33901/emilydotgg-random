use chrono::Utc;
use derive_more::{Deref, DerefMut};
use parking_lot::{Condvar, Mutex};
use std::io::{self, Read};
use std::panic::RefUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use fpsdk::plugin::{self, Info, InfoBuilder, StateReader, StateWriter};
use fpsdk::plugin::{message, PluginProxy};
use fpsdk::{create_plugin, AsRawPtr, MidiMessage, ProcessParamFlags, TimeFormat, ValuePtr};

static ONCE: Once = Once::new();
const LOG_PATH: &str = "S:/emilydotgg-random.log";

mod ui;

#[cfg(debug_assertions)]
pub(crate) const CONTROLLER_COUNT: u32 = 10;

#[cfg(not(debug_assertions))]
pub(crate) const CONTROLLER_COUNT: u32 = 100;

#[derive(Debug, Deref, DerefMut)]
struct Plugin {
    host: Host,
    tag: plugin::Tag,

    #[deref]
    #[deref_mut]
    inner: Arc<PluginShared>,
    ui_send: mpsc::Sender<ui::UIMessage>,
    ui_receive: Option<mpsc::Receiver<ui::UIMessage>>,
    host_send: mpsc::Sender<ui::HostMessage>,
    host_receive: mpsc::Receiver<ui::HostMessage>,
    proxy: Option<PluginProxy>,
    ui_thread: Option<JoinHandle<()>>,
}

pub(crate) fn interpolate<T>() {}

/// Plugin shared state
#[derive(Debug)]
struct PluginShared {
    inputs: Mutex<PluginInputs>,
    controller_values: Mutex<Option<Vec<(usize, f32)>>>,
    generators: Mutex<Vec<Generator>>,
    tick_happened: (Mutex<bool>, Condvar),
    tick: AtomicU64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
enum Generator {
    /// Random value
    Random,
    /// Random value, interpolating towards
    RandomInter {
        // Where to get to
        target_value: f32,
        target_tick: u64,

        // Where we are coming from
        initial_value: f32,
        initial_tick: u64,
    },
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct PluginInputs {
    seed: u64,
    speed: u64,
    test_val: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
enum SaveState {
    Ver1 {
        inputs: PluginInputs,
        generators: Vec<Generator>,
    },
}

impl fpsdk::plugin::Plugin for Plugin {
    fn new(host: Host, tag: plugin::Tag) -> Self {
        init_log();

        info!("init plugin with tag {}", tag);

        let plugin_shared = Arc::new(PluginShared {
            inputs: Mutex::new(PluginInputs {
                speed: 400,
                ..Default::default()
            }),
            controller_values: Mutex::new(Option::default()),
            generators: Mutex::new(vec![
                Generator::RandomInter {
                    target_value: 0.0,
                    target_tick: 1,
                    initial_value: 0.0,
                    initial_tick: 0,
                };
                CONTROLLER_COUNT as usize
            ]),
            tick_happened: (Mutex::new(false), Condvar::new()),
            tick: 1.into(),
        });

        let weak_state = Arc::downgrade(&plugin_shared);

        let (ui_send, ui_receive) = mpsc::channel(100);
        let (host_send, host_receive) = mpsc::channel(100);

        let gen_ui_send = ui_send.clone();

        let gen_thread_handle = std::thread::spawn(move || {
            info!("Starting Gen thread!");

            while let Some(state) = weak_state.upgrade() {
                // Wait for a tick to happen before doing anything
                {
                    let (ref tick_lock, ref cvar) = state.tick_happened;
                    let mut tick = tick_lock.lock();
                    while !*tick {
                        cvar.wait(&mut tick);
                    }
                    *tick = false;
                }

                let mut inputs = state.inputs.lock();

                if inputs.speed == 0 {
                    inputs.speed = 1;
                }

                let mut rng = thread_rng();
                let this_tick = state.tick.load(Ordering::Acquire);
                let is_speed_tick = this_tick % inputs.speed == 0;

                let mut generators = state.generators.lock();
                let mut values: Vec<(usize, f32)> = vec![];

                for (i, generator) in generators.iter_mut().enumerate() {
                    match generator {
                        Generator::Random => {
                            if is_speed_tick {
                                values.push((i, rng.gen_range(0.0..1.0)));
                            }
                        }
                        Generator::RandomInter {
                            target_value,
                            target_tick,
                            initial_tick,
                            initial_value,
                        } => {
                            if this_tick >= *target_tick {
                                *initial_value = *target_value;
                                *initial_tick = this_tick;
                                *target_tick = this_tick + inputs.speed;
                                *target_value = rng.gen_range(0.0..1.0);
                            }

                            let ratio = (this_tick - *initial_tick) as f32
                                / (*target_tick - *initial_tick) as f32;
                            let delta = *target_value - *initial_value;

                            values.push((i, (*initial_value + (ratio * delta))));
                        }
                    }
                }

                // Store values
                (*state.controller_values.lock()).replace(values.clone());
                let time = Utc::now();
                let ui_message = UIMessage::UpdateControllers((time, values));
                gen_ui_send.blocking_send(ui_message).unwrap();
            }

            info!("Gen thread done");
        });

        Self {
            host,
            tag,
            ui_send,
            ui_receive: Some(ui_receive),
            host_send,
            host_receive,
            proxy: None,
            ui_thread: None,
            inner: plugin_shared,
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
        let state = SaveState::Ver1 {
            inputs: self.inputs.lock().clone(),
            generators: self.generators.lock().clone(),
        };

        match bincode::serialize_into(writer, &state) {
            Ok(_) => info!("state {:?} saved", self.inputs),
            Err(e) => error!("error serializing state {}", e),
        }
    }

    fn load_state(&mut self, mut reader: StateReader) {
        let mut buf = [0; std::mem::size_of::<PluginInputs>()];
        reader
            .read(&mut buf)
            .and_then(|_| {
                bincode::deserialize::<SaveState>(&buf).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        format!("error deserializing value {}", e),
                    )
                })
            })
            .and_then(|value| {
                info!("Read state {:?}", value);
                match value {
                    SaveState::Ver1 {
                        inputs,
                        mut generators,
                    } => {
                        for gen in &mut generators {
                            if let Generator::RandomInter { target_tick, .. } = gen {
                                *target_tick = 0;
                            }
                        }

                        *self.inputs.lock() = inputs;
                        *self.generators.lock() = generators;
                    }
                }

                Ok(())
            })
            .unwrap_or_else(|e| error!("error reading value from state {}", e));
    }

    fn on_message(&mut self, message: host::Message) -> Box<dyn AsRawPtr> {
        // info!("Plugin got {:?}", message);
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
            let (ref lock, ref cvar) = self.tick_happened;
            let mut tick = lock.lock();
            *tick = true;
            cvar.notify_one();
        }

        self.tick.fetch_add(1, Ordering::Release);

        let controller_values = &self.inner.controller_values;

        // Inform FL about all our values, if they have changed
        controller_values.lock().take().map(|v| {
            for (i, v) in v.into_iter() {
                self.host
                    .on_controller(self.tag, i, ((v * 0x1000 as f32) as u64) << 32)
            }
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

            self.inputs.lock().speed = speed as u64;
        }

        Box::new(0)
    }

    fn midi_in(&mut self, message: MidiMessage) {
        trace!("receive MIDI message {:?}", message);
    }
}

impl RefUnwindSafe for Plugin {}

impl Plugin {
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

impl Drop for Plugin {
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

        info!("Goodbye!");
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

create_plugin!(Plugin);

use std::panic;

use crate::ui::UIMessage;

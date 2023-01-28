use chrono::Utc;
use derive_more::{Deref, DerefMut};
use fpsdk::plugin::message::ActivateMidi;
use fpsdk::voice::{ReceiveVoiceHandler, SendVoiceHandler};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
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
    voices: HashMap<fpsdk::voice::Tag, Box<Voice>>,
}

/// Plugin shared state
#[derive(Debug)]
struct PluginShared {
    inputs: Mutex<PluginInputs>,
    controller_values: Mutex<Option<Vec<(usize, f32)>>>,
    generators: Mutex<Vec<Generator>>,
    tick_happened: (Mutex<bool>, Condvar),
    tick: AtomicU64,
    editor_visible: Mutex<bool>,
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum GeneratorType {
    Random,
    RandomInter,
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
            editor_visible: Mutex::new(false),
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

                // TODO(emily): Here we need to be able to send 'values' to the UI thread that will
                // happen in the future... i.e. with RandomInter we are representing a straight line
                // with lots of smaller values (1 per tick ish). Instead of that we should just be
                // able to send the end point, and what time that end point should appear at.
                // That way we dont send so many fucking values every tick when its completely
                // unnecessary to do so. This would also SIGNIFICANTLY improve the perf of the UI by
                // enabling it to not have to plot 1000k values all  the fucking time.

                // Values that are given to FL studio
                let mut controller_values: Vec<(usize, f32)> = vec![];
                controller_values.reserve(generators.len());
                // Values that are given to the UI to render
                let mut ui_values: Vec<(usize, f32)> = vec![];
                ui_values.reserve(generators.len());

                for (i, generator) in generators.iter_mut().enumerate() {
                    match generator {
                        Generator::Random => {
                            if is_speed_tick {
                                let value = rng.gen_range(0.0..1.0);
                                controller_values.push((i, value));
                                ui_values.push((i, value))
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
                                ui_values.push((i, *target_value));
                            }

                            let ratio = (this_tick - *initial_tick) as f32
                                / (*target_tick - *initial_tick) as f32;
                            let delta = *target_value - *initial_value;

                            controller_values.push((i, (*initial_value + (ratio * delta))));
                        }
                    }
                }

                // Store values
                (*state.controller_values.lock()).replace(controller_values.clone());

                let time = Utc::now();
                if *state.editor_visible.lock() {
                    let ui_message = UIMessage::UpdateControllers((time, ui_values));
                    // TODO(emily): probably want to try_send here instead of blocking
                    gen_ui_send.blocking_send(ui_message).unwrap();
                }
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
            voices: HashMap::new(),
        }
    }

    fn proxy(&mut self, handle: plugin::PluginProxy) {
        self.proxy = Some(handle);
    }

    fn info(&self) -> Info {
        info!("plugin {} will return info", self.tag);

        let info = InfoBuilder::new_effect("emilydotgg-random", "Random", 3)
            .want_new_tick()
            .with_out_ctrls(100)
            .with_out_voices(100)
            // .no_process()
            // .midi_out()
            // .with_poly(16)
            .get_note_input()
            .build();

        return info;
    }

    fn save_state(&mut self, writer: StateWriter) {
        let state = SaveState::Ver1 {
            inputs: self.inputs.lock().clone(),
            generators: self.generators.lock().clone(),
        };

        match bincode::serialize_into(writer, &state) {
            Ok(_) => info!("state {:?} saved", state),
            Err(e) => error!("error serializing state {}", e),
        }
    }

    fn load_state(&mut self, mut reader: StateReader) {
        let mut buf: Vec<u8> = vec![];
        reader
            .read_to_end(&mut buf)
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
                            if let Generator::RandomInter {
                                target_tick,
                                initial_tick,
                                ..
                            } = gen
                            {
                                *target_tick = 1;
                                *initial_tick = 0;
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
        info!("Plugin got {:?}", message);
        match message {
            host::Message::SetEnabled(enabled) => {
                self.on_set_enabled(enabled, message);
            }
            host::Message::ShowEditor(handle) => {
                self.ui_send
                    .blocking_send(ui::UIMessage::ShowEditor(handle))
                    .unwrap();

                *self.inner.editor_visible.lock() = handle.is_some();
            }
            _ => (),
        }

        // TODO(emily): Doing this here is a little questionable? We want to always be trying to process these,
        // not just when FL sends us a message....
        while let Ok(response) = self.host_receive.try_recv() {
            info!("Host got {response:?}");
            match response {
                ui::HostMessage::SetEditorHandle(hwnd) => {
                    self.proxy.as_ref().unwrap().set_editor_hwnd(hwnd.unwrap());
                }
                ui::HostMessage::ChangeGenerators(to_what) => {
                    self.host.on_message(
                        self.tag,
                        message::DebugLogMsg(format!("changing generators to {:?}", to_what)),
                    );

                    let mut generators = self.generators.lock();
                    for gen in &mut *generators {
                        *gen = match to_what {
                            GeneratorType::Random => Generator::Random,
                            GeneratorType::RandomInter => Generator::RandomInter {
                                target_value: 0.0,
                                target_tick: 1,
                                initial_value: 0.0,
                                initial_tick: 0,
                            },
                        }
                    }
                }
            }
        }

        Box::new(0)
    }

    fn name_of(&self, message: GetName) -> String {
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

        let controller_values = { controller_values.lock().take() };
        controller_values.map(|v| {
            for (i, v) in v.into_iter() {
                self.host
                    .on_controller(self.tag, i, ((v * 0x1000 as f32) as u64) << 32)
            }
        });

        // self.host.midi_out(self.tag)
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
            // Scale speed into a more appropriate range
            // it will be 0 - 65535 coming in and we want it to be less

            let speed = value.get::<u16>() as f64;
            let speed = (speed / 65535.0) * 600.0;

            self.inputs.lock().speed = speed as u64;
        }

        Box::new(0)
    }

    fn midi_in(&mut self, message: MidiMessage) {
        info!("receive MIDI message {:?}", message);
    }

    fn voice_handler(&mut self) -> Option<&mut dyn ReceiveVoiceHandler> {
        Some(self)
    }

    fn render(&mut self, input: &[[f32; 2]], output: &mut [[f32; 2]]) {
        // consider it an effect
        input.iter().zip(output).for_each(|(inp, outp)| {
            outp[0] = inp[0]; // * 0.25;
            outp[1] = inp[1]; // * 0.25;
        });
    }
}

#[derive(Debug, Clone)]
struct Voice {
    tag: fpsdk::voice::Tag,
}

impl fpsdk::voice::Voice for Voice {
    fn tag(&self) -> fpsdk::voice::Tag {
        self.tag
    }
}

impl ReceiveVoiceHandler for Plugin {
    fn trigger(
        &mut self,
        params: fpsdk::voice::Params,
        tag: fpsdk::voice::Tag,
    ) -> &mut dyn fpsdk::voice::Voice {
        info!("ReceiveVoiceHandler::trigger");

        let voice = Box::new(Voice { tag });
        self.voices.insert(tag, voice);

        self.voices.get_mut(&tag).unwrap().as_mut()
    }

    fn release(&mut self, tag: fpsdk::voice::Tag) {
        info!("ReceiveVoiceHandler::release");
        self.voices.remove(&tag);
    }

    fn kill(&mut self, tag: fpsdk::voice::Tag) {
        self.voices.remove(&tag);
        info!("ReceiveVoiceHandler::kill");
    }
}

impl SendVoiceHandler for Plugin {
    fn kill(&mut self, tag: fpsdk::voice::Tag) {
        info!("kill voice {tag}");
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

        // self.host.on_message(self.tag, ActivateMidi);
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

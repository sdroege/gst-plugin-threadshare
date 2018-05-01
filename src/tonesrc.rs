// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use glib;
use glib::prelude::*;
use gst;
use gst::prelude::*;
use gst_audio;

use gobject_subclass::object::*;
use gst_plugin::element::*;

use std::sync::Mutex;
use std::time;
use std::{i32, u16, u32};

use futures::future;
use futures::sync::oneshot;
use futures::{Future, Stream};

use tokio::timer;

use either::Either;

use rand;

use muldiv::MulDiv;

use byte_slice_cast::*;

use iocontext::*;

const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_THREADS: i32 = 0;
const DEFAULT_CONTEXT_WAIT: u32 = 0;
const DEFAULT_SAMPLES_PER_BUFFER: u32 = 160;
const DEFAULT_FREQ1: u32 = 0;
const DEFAULT_VOL1: i32 = 0;
const DEFAULT_FREQ2: u32 = 0;
const DEFAULT_VOL2: i32 = 0;
const DEFAULT_ON_TIME1: u32 = 1000;
const DEFAULT_ON_TIME2: u32 = 1000;
const DEFAULT_OFF_TIME1: u32 = 1000;
const DEFAULT_OFF_TIME2: u32 = 1000;
const DEFAULT_REPEAT: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_threads: i32,
    context_wait: u32,
    samples_per_buffer: u32,
    tone_gen_settings: tonegen::ToneGenSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_threads: DEFAULT_CONTEXT_THREADS,
            context_wait: DEFAULT_CONTEXT_WAIT,
            samples_per_buffer: DEFAULT_SAMPLES_PER_BUFFER,
            tone_gen_settings: Default::default(),
        }
    }
}

static PROPERTIES: [Property; 13] = [
    Property::String(
        "context",
        "Context",
        "Context name to share threads with",
        Some(DEFAULT_CONTEXT),
        PropertyMutability::ReadWrite,
    ),
    Property::Int(
        "context-threads",
        "Context Threads",
        "Number of threads for the context thread-pool if we create it",
        (-1, u16::MAX as i32),
        DEFAULT_CONTEXT_THREADS,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "context-wait",
        "Context Wait",
        "Throttle poll loop to run at most once every this many ms",
        (0, 1000),
        DEFAULT_CONTEXT_WAIT,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "samples-per-buffer",
        "Samples Per Buffer",
        "Number of samples per output buffer",
        (1, u32::MAX),
        DEFAULT_SAMPLES_PER_BUFFER,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "freq1",
        "Frequency 1",
        "Frequency of first telephony tone component",
        (0, 4000),
        DEFAULT_FREQ1,
        PropertyMutability::ReadWrite,
    ),
    Property::Int(
        "vol1",
        "Volume 1",
        "Volume of first telephony tone component",
        (-50, 0),
        DEFAULT_VOL1,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "freq2",
        "Frequency 2",
        "Frequency of second telephony tone component",
        (0, 4000),
        DEFAULT_FREQ2,
        PropertyMutability::ReadWrite,
    ),
    Property::Int(
        "vol2",
        "Volume 2",
        "Volume of second telephony tone component",
        (-50, 0),
        DEFAULT_VOL2,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "on-time1",
        "On Time 1",
        "Time of the first period when the tone signal is present",
        (0, u32::MAX),
        DEFAULT_ON_TIME1,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "off-time1",
        "Off Time 1",
        "Time of the first period when the tone signal is off",
        (0, u32::MAX),
        DEFAULT_OFF_TIME1,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "on-time2",
        "On Time 2",
        "Time of the second period when the tone signal is present",
        (0, u32::MAX),
        DEFAULT_ON_TIME2,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "off-time2",
        "Off Time 2",
        "Time of the second period when the tone signal is off",
        (0, u32::MAX),
        DEFAULT_OFF_TIME2,
        PropertyMutability::ReadWrite,
    ),
    Property::Boolean(
        "repeat",
        "Repeat",
        "Whether to repeat specified tone indefinitly",
        DEFAULT_REPEAT,
        PropertyMutability::ReadWrite,
    ),
];

struct State {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    cancel: Option<oneshot::Sender<()>>,
    pending_future_cancel: Option<oneshot::Sender<()>>,
    need_initial_events: bool,
    buffer_pool: Option<gst::BufferPool>,
    sample_offset: u64,
    start_time: gst::ClockTime,
    tone_gen: Option<(tonegen::ToneGen, tonegen::ToneGenSettings)>,
}

impl Default for State {
    fn default() -> State {
        State {
            io_context: None,
            pending_future_id: None,
            cancel: None,
            pending_future_cancel: None,
            need_initial_events: true,
            buffer_pool: None,
            sample_offset: 0,
            start_time: gst::CLOCK_TIME_NONE,
            tone_gen: None,
        }
    }
}

struct ToneSrc {
    cat: gst::DebugCategory,
    src_pad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl ToneSrc {
    fn class_init(klass: &mut ElementClass) {
        klass.set_metadata(
            "Thread-sharing tone source",
            "Source/Generic",
            "Thread-sharing tone source",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_simple(
            "audio/x-raw",
            &[
                ("format", &gst_audio::AUDIO_FORMAT_S16.to_string()),
                ("layout", &"interleaved"),
                ("rate", &8000i32),
                ("channels", &1i32),
            ],
        );

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn init(element: &Element) -> Box<ElementImpl<Element>> {
        let templ = element.get_pad_template("src").unwrap();
        let src_pad = gst::Pad::new_from_template(&templ, "src");

        src_pad.set_event_function(|pad, parent, event| {
            ToneSrc::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_event(pad, element, event),
            )
        });
        src_pad.set_query_function(|pad, parent, query| {
            ToneSrc::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_query(pad, element, query),
            )
        });
        element.add_pad(&src_pad).unwrap();

        Box::new(Self {
            cat: gst::DebugCategory::new(
                "ts-tonesrc",
                gst::DebugColorFlags::empty(),
                "Thread-sharing tone source",
            ),
            src_pad: src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        })
    }

    fn create_io_context_event(state: &State) -> Option<gst::Event> {
        if let (&Some(ref pending_future_id), &Some(ref io_context)) =
            (&state.pending_future_id, &state.io_context)
        {
            let s = gst::Structure::new(
                "ts-io-context",
                &[
                    ("io-context", &glib::AnySendValue::new(io_context.clone())),
                    (
                        "pending-future-id",
                        &glib::AnySendValue::new(*pending_future_id),
                    ),
                ],
            );
            Some(gst::Event::new_custom_downstream_sticky(s).build())
        } else {
            None
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
                true
            }
            EventView::FlushStop(..) => {
                let (ret, state, pending) = element.get_state(0.into());
                if ret == gst::StateChangeReturn::Success && state == gst::State::Playing
                    || ret == gst::StateChangeReturn::Async && pending == gst::State::Playing
                {
                    let _ = self.start(element);
                }
                true
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(self.cat, obj: pad, "Handled event {:?}", event);
        } else {
            gst_log!(self.cat, obj: pad, "Didn't handle event {:?}", event);
        }

        ret
    }

    fn src_query(&self, pad: &gst::Pad, _element: &Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);
        let ret = match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                q.set(true, 0.into(), 0.into());
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let caps = pad.get_pad_template_caps().unwrap();
                let result = q.get_filter()
                    .map(|f| f.intersect_with_mode(&caps, gst::CapsIntersectMode::First))
                    .unwrap_or(caps.clone());
                q.set_result(&result);

                true
            }
            _ => false,
        };

        if ret {
            gst_log!(self.cat, obj: pad, "Handled query {:?}", query);
        } else {
            gst_log!(self.cat, obj: pad, "Didn't handle query {:?}", query);
        }
        ret
    }

    fn timeout(
        &self,
        element: &Element,
    ) -> future::Either<
        Box<Future<Item = (), Error = ()> + Send + 'static>,
        future::FutureResult<(), ()>,
    > {
        let mut events = Vec::new();
        let mut state = self.state.lock().unwrap();
        if state.need_initial_events {
            gst_debug!(self.cat, obj: element, "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            events.push(gst::Event::new_stream_start(&stream_id).build());

            events
                .push(gst::Event::new_caps(&self.src_pad.get_pad_template_caps().unwrap()).build());
            events.push(
                gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
            );

            if let Some(event) = Self::create_io_context_event(&state) {
                events.push(event);

                // Get rid of reconfigure flag
                self.src_pad.check_reconfigure();
            }
            state.need_initial_events = false;
        } else if self.src_pad.check_reconfigure() {
            if let Some(event) = Self::create_io_context_event(&state) {
                events.push(event);
            }
        }

        let buffer_pool = match state.buffer_pool {
            Some(ref pool) => pool.clone(),
            None => return future::Either::B(future::err(())),
        };

        drop(state);

        for event in events {
            self.src_pad.push_event(event);
        }

        let res = {
            match buffer_pool.acquire_buffer(None) {
                Err(err) => err.into_result().map(|_| ()),
                Ok(mut buffer) => {
                    {
                        let buffer = buffer.get_mut().unwrap();

                        let settings = self.settings.lock().unwrap().clone();
                        let mut state = self.state.lock().unwrap();

                        match &mut state.tone_gen {
                            &mut Some((_, ref old_settings))
                                if *old_settings == settings.tone_gen_settings =>
                            {
                                ()
                            }
                            &mut Some((ref mut tone_gen, ref mut old_settings)) => {
                                tone_gen.update(&settings.tone_gen_settings);
                                *old_settings = settings.tone_gen_settings.clone();
                            }
                            tone_gen => {
                                *tone_gen = Some((
                                    tonegen::ToneGen::new(&settings.tone_gen_settings),
                                    settings.tone_gen_settings,
                                ));
                            }
                        }

                        let samples = {
                            let mut data = buffer.map_writable().unwrap();
                            let data = data.as_mut_slice();
                            let data = data.as_mut_slice_of::<i16>().unwrap();
                            let tone_gen = state.tone_gen.as_mut().unwrap();
                            tone_gen.0.generate(data)
                        };

                        let timestamp = state.start_time
                            + gst::SECOND
                                .mul_div_floor(state.sample_offset, 8000)
                                .unwrap();
                        state.sample_offset += samples as u64;
                        buffer.set_pts(timestamp);
                        buffer.set_size((2 * samples) as usize);
                    }

                    gst_log!(self.cat, obj: element, "Forwarding buffer {:?}", buffer);
                    self.src_pad.push(buffer).into_result().map(|_| ())
                }
            }
        };

        let res = match res {
            Ok(_) => {
                gst_log!(self.cat, obj: element, "Successfully pushed item");
                Ok(())
            }
            Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => {
                gst_debug!(self.cat, obj: element, "EOS");
                Err(())
            }
            Err(err) => {
                gst_error!(self.cat, obj: element, "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
                Err(())
            }
        };

        match res {
            Ok(()) => {
                let mut state = self.state.lock().unwrap();

                if let State {
                    io_context: Some(ref io_context),
                    pending_future_id: Some(ref pending_future_id),
                    ref mut pending_future_cancel,
                    ..
                } = *state
                {
                    let (cancel, future) = io_context.drain_pending_futures(*pending_future_id);
                    *pending_future_cancel = cancel;

                    future
                } else {
                    future::Either::B(future::ok(()))
                }
            }
            Err(_) => future::Either::B(future::err(())),
        }
    }

    fn prepare(&self, element: &Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(self.cat, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        let io_context = IOContext::new(
            &settings.context,
            settings.context_threads as isize,
            settings.context_wait,
        ).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to create IO context: {}", err]
            )
        })?;

        let pending_future_id = io_context.acquire_pending_future_id();
        gst_debug!(
            self.cat,
            obj: element,
            "Got pending future id {:?}",
            pending_future_id
        );

        state.io_context = Some(io_context);
        state.pending_future_id = Some(pending_future_id);

        gst_debug!(self.cat, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Unpreparing");

        let mut state = self.state.lock().unwrap();

        if let (&Some(ref pending_future_id), &Some(ref io_context)) =
            (&state.pending_future_id, &state.io_context)
        {
            io_context.release_pending_future_id(*pending_future_id);
        }

        *state = State::default();

        gst_debug!(self.cat, obj: element, "Unprepared");

        Ok(())
    }

    fn start(&self, element: &Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(self.cat, obj: element, "Starting");

        let clock = element.get_clock();
        if clock != Some(gst::SystemClock::obtain()) {
            return Err(gst_error_msg!(
                gst::LibraryError::Settings,
                ["Only works if the system clock is used"]
            ));
        }
        let clock = clock.unwrap();

        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.lock().unwrap();

        let State {
            ref io_context,
            ref mut cancel,
            ref mut buffer_pool,
            ref mut start_time,
            ..
        } = *state;

        let caps = self.src_pad.get_pad_template_caps().unwrap();
        let pool = gst::BufferPool::new();
        let mut config = pool.get_config();
        config.set_params(Some(&caps), 2 * settings.samples_per_buffer, 0, 0);
        pool.set_config(config).map_err(|_| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool"]
            )
        })?;
        pool.set_active(true).map_err(|_| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to activate buffer pool"]
            )
        })?;
        *buffer_pool = Some(pool);

        let io_context = io_context.as_ref().unwrap();

        let (sender, receiver) = oneshot::channel();

        let element_clone = element.clone();
        let future = timer::Interval::new(time::Instant::now(), time::Duration::from_millis(20))
            .map(Either::Left)
            .map_err(|_| ())
            .select(receiver.map(Either::Right).map_err(|_| ()).into_stream())
            .for_each(move |item| {
                let tonesrc = element_clone.get_impl().downcast_ref::<ToneSrc>().unwrap();

                match item {
                    Either::Left(_) => tonesrc.timeout(&element_clone),
                    Either::Right(_) => {
                        gst_debug!(tonesrc.cat, obj: &element_clone, "Interrupted");
                        future::Either::B(future::err(()))
                    }
                }
            });

        io_context.spawn(future);
        *cancel = Some(sender);

        *start_time = clock.get_time() - element.get_base_time();

        gst_debug!(self.cat, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Stopping");
        let mut state = self.state.lock().unwrap();

        if let Some(pool) = state.buffer_pool.take() {
            let _ = pool.set_active(false);
        }
        let _ = state.cancel.take();
        let _ = state.pending_future_cancel.take();

        gst_debug!(self.cat, obj: element, "Stopped");

        Ok(())
    }
}

impl ObjectImpl<Element> for ToneSrc {
    fn set_property(&self, _obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value.get().unwrap_or_else(|| "".into());
            }
            Property::Int("context-threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_threads = value.get().unwrap();
            }
            Property::UInt("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get().unwrap();
            }
            Property::UInt("samples-per-buffer", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.samples_per_buffer = value.get().unwrap();
            }
            Property::UInt("freq1", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.freq1 = value.get().unwrap();
            }
            Property::Int("vol1", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.vol1 = value.get().unwrap();
            }

            Property::UInt("freq2", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.freq2 = value.get().unwrap();
            }
            Property::Int("vol2", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.vol2 = value.get().unwrap();
            }
            Property::UInt("on-time1", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.on_time1 = value.get().unwrap();
            }
            Property::UInt("off-time1", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.off_time1 = value.get().unwrap();
            }
            Property::UInt("on-time2", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.on_time2 = value.get().unwrap();
            }
            Property::UInt("off-time2", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.off_time2 = value.get().unwrap();
            }
            Property::Boolean("repeat", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tone_gen_settings.repeat = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            Property::Int("context-threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_threads.to_value())
            }
            Property::UInt("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            Property::UInt("samples-per-buffer", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.samples_per_buffer.to_value())
            }
            Property::UInt("freq1", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.freq1.to_value())
            }
            Property::Int("vol1", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.vol1.to_value())
            }
            Property::UInt("freq2", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.freq2.to_value())
            }
            Property::Int("vol2", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.vol2.to_value())
            }
            Property::UInt("on-time1", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.on_time1.to_value())
            }
            Property::UInt("off-time1", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.off_time1.to_value())
            }
            Property::UInt("on-time2", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.on_time2.to_value())
            }
            Property::UInt("off-time2", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.off_time2.to_value())
            }
            Property::Boolean("repeat", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.tone_gen_settings.repeat.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl<Element> for ToneSrc {
    fn change_state(
        &self,
        element: &Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => match self.prepare(element) {
                Err(err) => {
                    element.post_error_message(&err);
                    return gst::StateChangeReturn::Failure;
                }
                Ok(_) => (),
            },
            gst::StateChange::PlayingToPaused => match self.stop(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            gst::StateChange::ReadyToNull => match self.unprepare(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            _ => (),
        }

        let mut ret = element.parent_change_state(transition);
        if ret == gst::StateChangeReturn::Failure {
            return ret;
        }

        match transition {
            gst::StateChange::ReadyToPaused => {
                ret = gst::StateChangeReturn::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => match self.start(element) {
                Err(err) => {
                    element.post_error_message(&err);
                    return gst::StateChangeReturn::Failure;
                }
                Ok(_) => (),
            },
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                state.need_initial_events = true;
            }
            _ => (),
        }

        ret
    }
}

struct ToneSrcStatic;

impl ImplTypeStatic<Element> for ToneSrcStatic {
    fn get_name(&self) -> &str {
        "ToneSrc"
    }

    fn new(&self, element: &Element) -> Box<ElementImpl<Element>> {
        ToneSrc::init(element)
    }

    fn class_init(&self, klass: &mut ElementClass) {
        ToneSrc::class_init(klass);
    }
}

pub fn register(plugin: &gst::Plugin) {
    let type_ = register_type(ToneSrcStatic);
    gst::Element::register(plugin, "ts-tonesrc", 0, type_);
}

mod tonegen {
    use super::*;
    use std::os::raw::c_void;
    use std::ptr;

    #[repr(C)]
    struct ToneGenDescriptor(c_void);
    #[repr(C)]
    struct ToneGenState(c_void);

    pub struct ToneGen(ptr::NonNull<ToneGenState>, ptr::NonNull<ToneGenDescriptor>);

    #[derive(Debug, PartialEq, Eq, Clone)]
    pub struct ToneGenSettings {
        pub freq1: u32,
        pub vol1: i32,
        pub freq2: u32,
        pub vol2: i32,
        pub on_time1: u32,
        pub off_time1: u32,
        pub on_time2: u32,
        pub off_time2: u32,
        pub repeat: bool,
    }

    impl Default for ToneGenSettings {
        fn default() -> Self {
            Self {
                freq1: DEFAULT_FREQ1,
                vol1: DEFAULT_VOL1,
                freq2: DEFAULT_FREQ2,
                vol2: DEFAULT_VOL2,
                on_time1: DEFAULT_ON_TIME1,
                off_time1: DEFAULT_OFF_TIME1,
                on_time2: DEFAULT_ON_TIME2,
                off_time2: DEFAULT_OFF_TIME2,
                repeat: DEFAULT_REPEAT,
            }
        }
    }

    extern "C" {
        fn tone_gen_descriptor_init(
            ptr: *mut ToneGenDescriptor,
            freq1: i32,
            vol1: i32,
            freq2: i32,
            vol2: i32,
            on_time1: i32,
            off_time1: i32,
            on_time2: i32,
            off_time2: i32,
            repeat: i32,
        ) -> *mut ToneGenDescriptor;
        fn tone_gen_descriptor_free(ptr: *mut ToneGenDescriptor);

        fn tone_gen_init(ptr: *mut ToneGenState, desc: *mut ToneGenDescriptor)
            -> *mut ToneGenState;
        fn tone_gen_free(ptr: *mut ToneGenState);

        fn tone_gen(ptr: *mut ToneGenState, amp: *mut i16, max_samples: i32) -> i32;
    }

    impl ToneGen {
        pub fn new(settings: &ToneGenSettings) -> Self {
            unsafe {
                let ptr = ptr::NonNull::new(tone_gen_descriptor_init(
                    ptr::null_mut(),
                    settings.freq1 as i32,
                    settings.vol1,
                    settings.freq2 as i32,
                    settings.vol2,
                    settings.on_time1 as i32,
                    settings.off_time1 as i32,
                    settings.on_time2 as i32,
                    settings.off_time2 as i32,
                    if settings.repeat { 1 } else { 0 },
                )).unwrap();
                let ptr2 = ptr::NonNull::new(tone_gen_init(ptr::null_mut(), ptr.as_ptr())).unwrap();

                ToneGen(ptr2, ptr)
            }
        }

        pub fn update(&mut self, settings: &ToneGenSettings) {
            unsafe {
                let ptr = ptr::NonNull::new(tone_gen_descriptor_init(
                    self.1.as_ptr(),
                    settings.freq1 as i32,
                    settings.vol1,
                    settings.freq2 as i32,
                    settings.vol2,
                    settings.on_time1 as i32,
                    settings.off_time1 as i32,
                    settings.on_time2 as i32,
                    settings.off_time2 as i32,
                    if settings.repeat { 1 } else { 0 },
                )).unwrap();
                self.1 = ptr;

                let ptr2 =
                    ptr::NonNull::new(tone_gen_init(self.0.as_ptr(), self.1.as_ptr())).unwrap();
                self.0 = ptr2;
            }
        }

        pub fn generate(&mut self, amp: &mut [i16]) -> i32 {
            unsafe { tone_gen(self.0.as_ptr(), amp.as_mut_ptr(), amp.len() as i32) }
        }
    }

    impl Drop for ToneGen {
        fn drop(&mut self) {
            unsafe {
                tone_gen_descriptor_free(self.1.as_ptr());
                tone_gen_free(self.0.as_ptr());
            }
        }
    }

    unsafe impl Send for ToneGen {}
}

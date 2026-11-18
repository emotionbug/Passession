//! Paseo's opt-in main-loop diagnostic. The reporter sleeps off the UI thread so
//! even a completely idle event loop produces samples without being woken.

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Display,
    panic::Location,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{cx::Cx, event::Event};

pub(crate) type Owner = &'static Location<'static>;

#[derive(Default)]
struct Cost {
    count: usize,
    time: Duration,
}

impl Cost {
    fn add(&mut self, time: Duration) {
        self.count += 1;
        self.time += time;
    }
}

impl Display for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{:.3}ms", self.count, ms(self.time))
    }
}

#[derive(Default)]
struct Sample {
    events: BTreeMap<&'static str, Cost>,
    timers: BTreeMap<Option<Owner>, Cost>,
    next_frame_requests: BTreeMap<Owner, usize>,
    redraws: BTreeMap<Owner, usize>,
    layers: BTreeMap<Owner, (usize, usize)>,
    actions: usize,
    render_passes: usize,
    render_frames: usize,
    render_time: Duration,
    texture_uploads: usize,
    texture_upload_bytes: u64,
    surface_presents: usize,
    surface_repair_pct_sum: f64,
    surface_reuse_waits: usize,
    surface_deferred: usize,
    surface_publish_time: Duration,
}

pub(crate) struct FrameTrace {
    sample: Arc<Mutex<Sample>>,
    timers: HashMap<u64, (Owner, bool)>,
}

impl FrameTrace {
    pub(crate) fn new() -> Option<Box<Self>> {
        if std::env::var_os("PASEO_FRAME_TRACE").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        let sample = Arc::new(Mutex::new(Sample::default()));
        let reporter = Arc::downgrade(&sample);
        if let Err(error) = std::thread::Builder::new().name("paseo-frame-trace".into()).spawn(move || {
            let started = Instant::now();
            let mut previous = started;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(sample) = reporter.upgrade() else { break };
                let now = Instant::now();
                let sample = std::mem::take(&mut *sample.lock().unwrap());
                sample.report(now.duration_since(started), now.duration_since(previous));
                previous = now;
            }
        }) {
            eprintln!("PASEO_FRAME_TRACE: could not start reporter: {error}");
            return None;
        }
        Some(Box::new(Self { sample, timers: HashMap::new() }))
    }

    pub(crate) fn timer_owner(&mut self, event: &Event) -> Option<Owner> {
        let Event::Timer(event) = event else { return None };
        let (owner, repeats) = self.timers.get(&event.timer_id).copied()?;
        if !repeats {
            self.timers.remove(&event.timer_id);
        }
        Some(owner)
    }

    pub(crate) fn event(&self, event: &Event, owner: Option<Owner>, elapsed: Duration) {
        let mut sample = self.sample.lock().unwrap();
        sample.events.entry(event.name()).or_default().add(elapsed);
        match event {
            Event::Timer(_) => sample.timers.entry(owner).or_default().add(elapsed),
            Event::Actions(actions) => sample.actions += actions.len(),
            _ => (),
        }
    }

    pub(crate) fn start_timer(&mut self, id: u64, owner: Owner, repeats: bool) {
        self.timers.insert(id, (owner, repeats));
    }

    pub(crate) fn stop_timer(&mut self, id: u64) {
        self.timers.remove(&id);
    }

    pub(crate) fn redraw(&self, owner: Owner) {
        *self.sample.lock().unwrap().redraws.entry(owner).or_default() += 1;
    }

    pub(crate) fn next_frame(&self, owner: Owner) {
        *self.sample.lock().unwrap().next_frame_requests.entry(owner).or_default() += 1;
    }

    /// Native render/present time reported by the X11 and Apple window loops.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
    pub(crate) fn render(&self, passes: usize, elapsed: Duration) {
        let mut sample = self.sample.lock().unwrap();
        sample.render_passes += passes;
        sample.render_frames += usize::from(passes != 0);
        sample.render_time += elapsed;
    }

    pub(crate) fn texture_upload(&self, width: usize, height: usize, bytes_per_pixel: usize) {
        let mut sample = self.sample.lock().unwrap();
        sample.texture_uploads += 1;
        sample.texture_upload_bytes += width as u64 * height as u64 * bytes_per_pixel as u64;
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(crate) fn surface_deferred(&self, reuse_wait: bool) {
        let mut sample = self.sample.lock().unwrap();
        sample.surface_deferred += 1;
        sample.surface_reuse_waits += usize::from(reuse_wait);
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(crate) fn surface_presented(
        &self, [width, height]: [i32; 2], repair: crate::damage::DamageRect, elapsed: Duration,
    ) {
        let mut sample = self.sample.lock().unwrap();
        sample.surface_presents += 1;
        sample.surface_repair_pct_sum += 100.0 * f64::from(repair.x1 - repair.x0)
            * f64::from(repair.y1 - repair.y0) / (f64::from(width) * f64::from(height));
        sample.surface_publish_time += elapsed;
    }
}

impl Cx {
    #[doc(hidden)]
    pub fn frame_trace_layer(&self, kind: &'static Location<'static>, repaint: bool) {
        if let Some(trace) = &self.frame_trace {
            let mut sample = trace.sample.lock().unwrap();
            let counts = sample.layers.entry(kind).or_default();
            if repaint {
                counts.0 += 1;
            } else {
                counts.1 += 1;
            }
        }
    }
}

fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
}

fn series(rows: impl Iterator<Item = impl Display>) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for row in rows {
        if !text.is_empty() {
            text.push(',');
        }
        write!(text, "{row}").unwrap();
    }
    text
}

impl Sample {
    fn report(self, since_start: Duration, interval: Duration) {
        let event = |name| self.events.get(name).map_or(0, |cost| cost.count);
        let draw_time = self.events.get("Draw").map_or(Duration::ZERO, |cost| cost.time);
        let all_event_time: Duration = self.events.values().map(|cost| cost.time).sum();
        eprintln!(
            "PASEO_FRAME_TRACE t={:.3}s interval={:.3}s draw={} render_passes={} render_frames={} next_frame={} timer={} signal={} actions={}/{} event_ms={:.3} draw_ms={:.3} render_ms={:.3} texture_uploads={} texture_upload_bytes={} surface_presents={} surface_repair_pct={:.2} surface_reuse_waits={} surface_deferred={} surface_publish_ms={:.3} events=[{}] timers=[{}] next_frame_requests=[{}] layers=[{}] redraw=[{}]",
            since_start.as_secs_f64(), interval.as_secs_f64(), event("Draw"), self.render_passes, self.render_frames,
            event("NextFrame"), event("Timer"), event("Signal"), event("Actions"), self.actions,
            ms(all_event_time - draw_time), ms(draw_time), ms(self.render_time),
            self.texture_uploads, self.texture_upload_bytes,
            self.surface_presents, self.surface_repair_pct_sum / self.surface_presents.max(1) as f64,
            self.surface_reuse_waits, self.surface_deferred, ms(self.surface_publish_time),
            series(self.events.iter().map(|(name, cost)| format!("{name}={cost}"))),
            series(self.timers.iter().map(|(owner, cost)| {
                owner.map_or_else(|| format!("unknown={cost}"), |owner| format!("{owner}={cost}"))
            })),
            series(self.next_frame_requests.iter().map(|(owner, n)| format!("{owner}={n}"))),
            series(self.layers.iter().map(|(kind, (repaint, replay))| format!("{kind}={repaint}/{replay}"))),
            series(self.redraws.iter().map(|(owner, n)| format!("{owner}={n}"))),
        );
    }
}

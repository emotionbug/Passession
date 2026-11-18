//! One draw-armed clock for visible motion and content deadlines. Text/geometry
//! requests rearm on paint; retained shader instances rearm without a widget draw.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use makepad_widgets::makepad_draw::makepad_platform::makepad_shader_compiler::ShaderTy;
use makepad_widgets::*;

pub const RING_CADENCE: Duration = Duration::from_millis(125);
const UNFOCUSED_PERIOD: Duration = Duration::from_millis(250);
pub const MOTION_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 30);

#[derive(Clone, Copy, Debug)]
struct Deadline {
    when: Instant,
    area: Area,
    period: Option<Duration>,
    effect: Option<InstanceEffect>,
}

/// Only a surface that is guaranteed opaque may submit a cover. Use its
/// conservative interior (exclude rounded corners), in the list that paints it.
pub fn occlude(cx: &mut Cx2d, rect: Rect) {
    if rect.size.x <= 0.0 || rect.size.y <= 0.0 {
        return;
    }
    let mut area = Area::Empty;
    cx.add_rect_area(&mut area, rect);
    CLOCK.with(|clock| {
        if let Some(list) = area.draw_list_id() {
            clock.borrow_mut().covers.insert(list, area);
        }
    });
}

#[derive(Default)]
struct PaintOrder {
    lists: HashMap<DrawListId, (DrawListId, usize)>,
    covers: Vec<(DrawListId, usize, Rect)>,
}

impl PaintOrder {
    fn gather(&mut self, cx: &Cx, covers: &HashMap<DrawListId, Area>) {
        self.lists.clear();
        self.covers.clear();
        for area in covers.values() {
            let list = area.draw_list_id().unwrap();
            if !self.lists.contains_key(&list)
                && let Some(pass) = cx.draw_lists[list].pass_id
                && let Some(root) = cx.passes[pass].main_draw_list_id
            {
                self.visit(cx, root, root);
            }
            if let Some(&(root, z)) = self.lists.get(&list) {
                self.covers.push((root, z, area.clipped_rect(cx)));
            }
        }
    }

    fn visit(&mut self, cx: &Cx, root: DrawListId, id: DrawListId) {
        if self.lists.contains_key(&id) {
            return;
        }
        let Some(list) = cx.draw_lists.checked_index(id) else {
            return;
        };
        let z = self.lists.len();
        self.lists.insert(id, (root, z));
        for index in 0..list.draw_items.len() {
            if let Some(child) = list.draw_items[index].sub_list() {
                self.visit(cx, root, child);
            }
        }
    }

    fn suppresses(&self, cx: &Cx, area: Area) -> bool {
        let rect = area.clipped_rect(cx);
        if rect.size.x <= 0.0 || rect.size.y <= 0.0 {
            return true;
        }
        let Some(&(root, z)) = area.draw_list_id().and_then(|id| self.lists.get(&id)) else {
            return false;
        };
        self.covers.iter().any(|&(owner, cover_z, cover)| {
            owner == root
                && cover_z > z
                && rect.pos.x >= cover.pos.x
                && rect.pos.y >= cover.pos.y
                && rect.pos.x + rect.size.x <= cover.pos.x + cover.size.x
                && rect.pos.y + rect.size.y <= cover.pos.y + cover.size.y
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum InstanceEffect {
    Spin {
        field: LiveId,
        cycle_ms: f64,
        offset: f64,
    },
    Sample(fn(&mut Cx, Area, u128) -> bool),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Target {
    Repaint(DrawListId, bool),
    Phase(Area),
}

impl Deadline {
    fn target(&self) -> Target {
        if self.effect.is_some() {
            Target::Phase(self.area)
        } else {
            Target::Repaint(self.area.draw_list_id().unwrap(), self.period.is_some())
        }
    }
}

#[derive(Default)]
struct Schedule {
    entries: HashMap<Target, Deadline>,
    ordered: BTreeSet<(Instant, Target)>,
}

impl Schedule {
    fn insert(&mut self, deadline: Deadline) {
        let target = deadline.target();
        if let Some(old) = self.entries.get(&target) {
            if old.area.redraw_id() == deadline.area.redraw_id() && old.when <= deadline.when {
                return;
            }
            self.ordered.remove(&(old.when, target));
        }
        self.entries.insert(target, deadline);
        self.ordered.insert((deadline.when, target));
    }

    fn remove(&mut self, target: Target) -> Option<Deadline> {
        let old = self.entries.remove(&target)?;
        self.ordered.remove(&(old.when, target));
        Some(old)
    }

    fn next(&self) -> Option<Instant> {
        self.ordered.first().map(|&(when, _)| when)
    }

    fn pop_due(&mut self, now: Instant) -> Option<Deadline> {
        let &(when, target) = self.ordered.first()?;
        if when > now {
            return None;
        }
        self.remove(target)
    }
}

struct Clock {
    parked: HashMap<Target, Deadline>,
    covers: HashMap<DrawListId, Area>,
    paint_order: PaintOrder,
    schedule: Schedule,
    timer: Timer,
    armed: Option<Instant>,
    epoch: Instant,
    focused: bool,
    hidden: bool,
}

impl Clock {
    fn new() -> Self {
        Self {
            schedule: Schedule::default(),
            timer: Timer::default(),
            parked: HashMap::new(),
            covers: HashMap::new(),
            paint_order: PaintOrder::default(),
            armed: None,
            epoch: Instant::now(),
            focused: true,
            hidden: false,
        }
    }

    fn next_step(&self, now: Instant, period: Duration) -> Instant {
        let period = if self.focused {
            period
        } else {
            period.max(UNFOCUSED_PERIOD)
        };
        // Align to a shared grid: indicators drawn at slightly different times
        // should not schedule a burst of independent timers for each animation step.
        let period = period.as_nanos() as u64;
        let offset =
            (now.saturating_duration_since(self.epoch).as_nanos() % u128::from(period)) as u64;
        now + Duration::from_nanos(period - offset)
    }

    fn arm(&mut self, cx: &mut Cx, now: Instant) {
        let next = if self.hidden {
            None
        } else {
            self.schedule.next()
        };
        if next == self.armed {
            return;
        }
        if self.armed.is_some() {
            cx.stop_timer(self.timer);
        }
        self.armed = next;
        self.timer = next.map_or_else(Timer::default, |when| {
            // Platform timers can truncate sub-ms intervals. Round up so a timer
            // for an exact visible boundary cannot fire on its old value.
            let delay = when
                .saturating_duration_since(now)
                .as_nanos()
                .div_ceil(1_000_000)
                .max(1);
            cx.with_timer_owner(super::APP_TIMERS, |cx| {
                cx.start_timeout(delay as f64 / 1000.0)
            })
        });
    }

    fn set_focus(&mut self, focused: bool, now: Instant) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        // Focus changes are rare. Rebucket only continuous motion, never exact
        // content/expiry deadlines, so the pending timer also throttles at once.
        let mut schedule = std::mem::take(&mut self.schedule);
        schedule.ordered.clear();
        for (&target, entry) in &mut schedule.entries {
            if let Some(period) = entry.period {
                entry.when = self.next_step(now, period);
            }
            schedule.ordered.insert((entry.when, target));
        }
        self.schedule = schedule;
    }

    fn fire(&mut self, cx: &mut Cx, mut entry: Deadline, now: Instant, wall_ms: u128) {
        if !entry.area.is_attached(cx) {
            return;
        }
        if self.paint_order.suppresses(cx, entry.area) {
            self.parked.insert(entry.target(), entry);
            return;
        }
        if let Some(effect) = entry.effect {
            let updated = match effect {
                InstanceEffect::Spin {
                    field,
                    cycle_ms,
                    offset,
                } => write_instance(
                    cx,
                    entry.area,
                    field,
                    rotation_at(wall_ms, cycle_ms, offset) as f32,
                ),
                InstanceEffect::Sample(sample) => sample(cx, entry.area, wall_ms),
            };
            if !updated {
                return;
            }
            // Writing the GPU instance sets pass.paint_dirty, not a widget Draw.
            // This registration survives retained replay until the instance detaches.
            entry.when = self.next_step(now, entry.period.unwrap());
            self.schedule.insert(entry);
        } else {
            cx.redraw_area(entry.area);
        }
    }

    fn finish_draw(&mut self, cx: &mut Cx, now: Instant) {
        self.covers.retain(|_, area| area.is_attached(cx));
        if self.parked.is_empty() && self.schedule.entries.is_empty() {
            self.arm(cx, now);
            return;
        }
        self.paint_order.gather(cx, &self.covers);
        self.parked.retain(|&target, entry| {
            if !entry.area.is_attached(cx) {
                return false;
            }
            if self.paint_order.suppresses(cx, entry.area) {
                return true;
            }
            // A retained area may have been covered for longer than its deadline.
            // One immediate wake refreshes it without waiting for a new model draw.
            entry.when = now;
            self.schedule.ordered.insert((now, target));
            self.schedule.entries.insert(target, *entry);
            false
        });
        self.schedule.entries.retain(|&target, entry| {
            let attached = entry.area.is_attached(cx);
            let suppressed = attached && self.paint_order.suppresses(cx, entry.area);
            if !attached || suppressed {
                self.schedule.ordered.remove(&(entry.when, target));
                if suppressed {
                    self.parked.insert(target, *entry);
                }
                return false;
            }
            true
        });
        self.arm(cx, now);
    }
}

thread_local! {
    static CLOCK: RefCell<Clock> = RefCell::new(Clock::new());
}

/// Register where an indicator was painted, not during measurement or on idle.
pub fn register_every(cx: &mut Cx, area: Area, period: Duration) {
    assert!(!period.is_zero() && period.as_nanos() <= u128::from(u64::MAX));
    schedule(cx, area, Some(period), None, None);
}

/// A one-shot repaint at the next actual content change (e.g. copy expiry).
pub fn register_until(cx: &mut Cx, area: Area, when: Instant) {
    schedule(cx, area, None, Some(when), None);
}

/// Drive a normalized phase on a retained float instance property.
pub fn spin(cx: &mut Cx, area: Area, field: LiveId, period: Duration, phase_ms: f64, offset: f64) {
    let effect = InstanceEffect::Spin {
        field,
        cycle_ms: phase_ms,
        offset,
    };
    schedule(cx, area, Some(period), None, Some(effect));
}

/// Sample several animated properties without a widget draw (e.g. arc ends).
pub fn sample(cx: &mut Cx, area: Area, period: Duration, update: fn(&mut Cx, Area, u128) -> bool) {
    schedule(
        cx,
        area,
        Some(period),
        None,
        Some(InstanceEffect::Sample(update)),
    );
}

pub fn write_instance(cx: &mut Cx, area: Area, field: LiveId, value: f32) -> bool {
    let Some(field) = area.get_write_ref(cx, field, ShaderTy::Float, "indicator clock") else {
        return false;
    };
    for i in 0..field.repeat {
        field.buffer[i * field.stride] = value;
    }
    true
}

pub fn rotation(phase_ms: f64, offset: f64) -> f64 {
    rotation_at(now_ms(), phase_ms, offset)
}

fn rotation_at(wall_ms: u128, phase_ms: f64, offset: f64) -> f64 {
    ((wall_ms as f64 % phase_ms) / phase_ms + offset).rem_euclid(1.0)
}

/// Minute/hour/day boundaries for relative age labels that become dates after a week.
pub(crate) fn relative_time_delay(at: Option<i64>, now_ms: i64) -> Option<Duration> {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    let elapsed = now_ms.saturating_sub(at?);
    let next = match elapsed {
        ..MINUTE => MINUTE.saturating_sub(elapsed),
        MINUTE..HOUR => MINUTE - elapsed % MINUTE,
        HOUR..DAY => HOUR - elapsed % HOUR,
        DAY..WEEK => DAY - elapsed % DAY,
        _ => return None,
    };
    Some(Duration::from_millis(next as u64))
}

pub(crate) fn register_relative_time(cx: &mut Cx2d, rect: Rect, at: Option<i64>, now_ms: i64) {
    if let Some(delay) = relative_time_delay(at, now_ms) {
        let mut area = Area::Empty;
        cx.add_rect_area(&mut area, rect);
        register_until(cx, area, Instant::now() + delay);
    }
}

fn schedule(
    cx: &mut Cx,
    area: Area,
    period: Option<Duration>,
    when: Option<Instant>,
    effect: Option<InstanceEffect>,
) {
    if area.is_empty() {
        return;
    }
    CLOCK.with(|clock| {
        let mut clock = clock.borrow_mut();
        let now = Instant::now();
        let when = when.unwrap_or_else(|| clock.next_step(now, period.unwrap()));
        let entry = Deadline {
            when,
            area,
            period,
            effect,
        };
        clock.parked.remove(&entry.target());
        clock.schedule.insert(entry);
        if !cx.in_draw_event() {
            clock.arm(cx, now);
        }
    });
}

/// Remove a registration when its retained paint detaches before the deadline.
pub fn cancel(cx: &mut Cx, area: Area) {
    if let Some(list) = area.draw_list_id() {
        CLOCK.with(|clock| {
            let mut clock = clock.borrow_mut();
            for target in [
                Target::Repaint(list, false),
                Target::Repaint(list, true),
                Target::Phase(area),
            ] {
                if clock
                    .schedule
                    .entries
                    .get(&target)
                    .is_some_and(|entry| entry.area.redraw_id() == area.redraw_id())
                {
                    clock.schedule.remove(target);
                }
                if clock
                    .parked
                    .get(&target)
                    .is_some_and(|entry| entry.area.redraw_id() == area.redraw_id())
                {
                    clock.parked.remove(&target);
                }
            }
            if !cx.in_draw_event() {
                clock.arm(cx, Instant::now());
            }
        });
    }
}

/// Milliseconds since the Unix epoch; indicators reading wall time stay in phase.
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Phase in `0..1` of a `period_ms` cycle.
pub fn phase(period_ms: u128) -> f64 {
    (now_ms() % period_ms) as f64 / period_ms as f64
}

/// The displayed state and `register_every` share the same step grid.
pub fn step(period: Duration, states: u128) -> usize {
    CLOCK.with(|clock| {
        let elapsed = clock.borrow().epoch.elapsed().as_nanos();
        ((elapsed / period.as_nanos()) % states) as usize
    })
}

/// Run after the final per-window overlay composition. Covered indicators keep
/// their retained state but do not arm the timer until the cover moves away.
pub fn finish_draw(cx: &mut Cx) {
    CLOCK.with(|clock| clock.borrow_mut().finish_draw(cx, Instant::now()));
}

/// Shell: forwards timer and lifecycle events.
pub fn handle_event(cx: &mut Cx, event: &Event) {
    if !matches!(
        event,
        Event::Timer(_)
            | Event::AppGotFocus
            | Event::AppLostFocus
            | Event::Background
            | Event::Pause
            | Event::Foreground
            | Event::Resume
    ) {
        return;
    }
    CLOCK.with(|clock| {
        let mut clock = clock.borrow_mut();
        if matches!(event, Event::Timer(_)) && clock.timer.is_event(event).is_none() {
            return;
        }
        let now = Instant::now();
        match event {
            Event::AppGotFocus => clock.set_focus(true, now),
            Event::AppLostFocus => clock.set_focus(false, now),
            Event::Background | Event::Pause => clock.hidden = true,
            Event::Foreground | Event::Resume => {
                if clock.hidden {
                    clock.hidden = false;
                    // Wall time advanced while hidden. Refresh live retained paint,
                    // then let visible redraws arm their next content deadline.
                    let mut schedule = std::mem::take(&mut clock.schedule);
                    let wall_ms = now_ms();
                    for (_, entry) in schedule.entries.drain() {
                        clock.fire(cx, entry, now, wall_ms);
                    }
                    schedule.entries.extend(clock.schedule.entries.drain());
                    std::mem::swap(&mut schedule.ordered, &mut clock.schedule.ordered);
                    clock.schedule = schedule;
                }
            }
            Event::Timer(_) => {
                clock.armed = None;
                clock.timer = Timer::default();
                let wall_ms = now_ms();
                while let Some(entry) = clock.schedule.pop_due(now) {
                    clock.fire(cx, entry, now, wall_ms);
                }
            }
            _ => unreachable!(),
        }
        clock.arm(cx, now);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(list: DrawListId, redraw_id: u64) -> Area {
        Area::Rect(RectArea {
            draw_list_id: list,
            rect_id: 0,
            redraw_id,
        })
    }

    #[test]
    fn indicator_clock_arms_only_the_earliest_due_areas() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let first = DrawList2d::new(&mut cx);
        let second = DrawList2d::new(&mut cx);
        let now = Instant::now();
        let early = now + Duration::from_secs(1);
        let late = now + Duration::from_secs(2);
        let mut clock = Clock::new();
        clock.arm(&mut cx, now);
        assert_eq!(clock.armed, None);
        for (when, list) in [(late, second.id()), (early, first.id())] {
            clock.schedule.insert(Deadline {
                when,
                area: area(list, 1),
                period: None,
                effect: None,
            });
            clock.arm(&mut cx, now);
            assert_eq!(clock.armed, Some(when));
        }
        assert!(clock.schedule.pop_due(now).is_none());
        assert_eq!(
            clock.schedule.pop_due(early).map(|entry| entry.area),
            Some(area(first.id(), 1))
        );
        assert!(clock.schedule.pop_due(early).is_none());
        clock.arm(&mut cx, early);
        assert_eq!(clock.armed, Some(late));
        assert_eq!(
            clock.schedule.pop_due(late).map(|entry| entry.area),
            Some(area(second.id(), 1))
        );
        clock.arm(&mut cx, late);
        assert_eq!(clock.armed, None);
    }

    #[test]
    fn indicator_clock_replaces_old_paint_and_keeps_one_wakeup() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let list = DrawList2d::new(&mut cx);
        let now = Instant::now();
        let expiry = now + Duration::from_millis(1500);
        let mut schedule = Schedule::default();
        for redraw_id in [1, 1, 2] {
            schedule.insert(Deadline {
                when: expiry,
                area: area(list.id(), redraw_id),
                period: None,
                effect: None,
            });
        }
        assert_eq!(schedule.next(), Some(expiry));
        assert_eq!(
            schedule.pop_due(expiry).map(|entry| entry.area),
            Some(area(list.id(), 2))
        );
        assert!(schedule.pop_due(expiry).is_none());
        assert_eq!(schedule.next(), None);
    }
}

#[cfg(test)]
mod paint_tests {
    use super::*;

    #[test]
    fn indicator_clock_spin_does_not_invalidate_host_and_stops_under_opaque_cover() {
        use makepad_widgets::makepad_draw::CxDraw;

        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_widgets::makepad_draw::live_design(&mut cx);
        crate::theme::live_design(&mut cx);
        crate::theme::DrawRing::register_main_module(&mut cx);
        cx.live_expand();
        let mut ring = crate::theme::DrawRing::new_local(&mut cx);
        let pass = Pass::new(&mut cx);
        let bounds = Rect {
            pos: dvec2(0.0, 0.0),
            size: dvec2(160.0, 210.0),
        };
        pass.set_size(&mut cx, bounds.size);
        let mut root = DrawList2d::new(&mut cx);
        let mut host = DrawList2d::new(&mut cx);
        let mut cover = DrawList2d::new(&mut cx);
        let mut cover_area = Area::Empty;
        let event = DrawEvent::default();
        {
            let mut draw = CxDraw::new(&mut cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            root.begin_always(&mut draw);
            let mut cx = Cx2d::new(&mut draw);
            cx.begin_pass_sized_turtle(Layout::default());
            host.begin_always(&mut cx);
            ring.rotation = -1.0;
            ring.draw_abs(
                &mut cx,
                Rect {
                    pos: dvec2(30.0, 60.0),
                    size: dvec2(12.0, 12.0),
                },
            );
            host.end(&mut cx);
            cover.begin_always(&mut cx);
            cx.add_rect_area(&mut cover_area, bounds);
            cover.end(&mut cx);
            cx.end_pass_sized_turtle();
            root.end(&mut cx);
            cx.end_pass(&pass);
        }

        let area = ring.area();
        let now = Instant::now();
        let entry = Deadline {
            when: now,
            area,
            period: Some(MOTION_PERIOD),
            effect: Some(InstanceEffect::Spin {
                field: live_id!(rotation),
                cycle_ms: 900.0,
                offset: 0.0,
            }),
        };
        let mut clock = Clock::new();
        cx.new_draw_event = DrawEvent::default();
        clock.fire(&mut cx, entry, now, 900);
        let phase = area
            .get_read_ref(&cx, live_id!(rotation), ShaderTy::Float)
            .unwrap();
        assert!((0.0..1.0).contains(&phase.buffer[0]));
        assert!(
            !cx.new_draw_event.will_redraw(),
            "phase update invalidated the host"
        );
        assert!(clock.schedule.next().is_some());

        clock.covers.insert(cover.id(), cover_area);
        clock.finish_draw(&mut cx, now);
        assert_eq!(clock.armed, None);
        assert_eq!(clock.parked.len(), 1);

        // Exposing the retained host (no new host paint) makes it eligible again.
        {
            let mut draw = CxDraw::new(&mut cx, &event);
            cover.release(&mut draw);
        }
        clock.finish_draw(&mut cx, now);
        assert_eq!(clock.armed, Some(now));
        assert!(clock.parked.is_empty());
        clock.hidden = true;
        clock.arm(&mut cx, now);
        assert_eq!(clock.armed, None);
    }
}

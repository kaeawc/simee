//! The sample history: a ring of one point per second, 15 minutes deep, and its 1/5/15-minute
//! averages. Time is an explicit `t` (seconds) on every point, so the arithmetic is tested without
//! a clock.

use std::collections::VecDeque;

use serde::Serialize;

use crate::{Level, ThermalState};

/// What one sample contributes to the history.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    /// Seconds since the sampler started.
    pub t: f64,
    pub load1: Option<f64>,
    pub free_percent: Option<f64>,
    pub level: Level,
    pub thermal: Option<ThermalState>,
    /// Cumulative Intel core throttle events (summed over CPUs), where the kernel counts them.
    pub throttle_total: Option<u64>,
    pub on_battery: Option<bool>,
}

/// The ring's capacity: 15 minutes at 1 s.
pub const CAPACITY: usize = 900;

/// The windows reported, in seconds: 1, 5 and 15 minutes.
pub const WINDOWS_S: [u32; 3] = [60, 300, 900];

#[derive(Clone, Debug)]
pub struct Ring {
    cap: usize,
    points: VecDeque<Point>,
}

/// Samples spent in each thermal state over a window (one sample is about one second).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ThermalTime {
    pub nominal: u32,
    pub fair: u32,
    pub serious: u32,
    pub critical: u32,
    /// Samples with no thermal reading.
    pub unknown: u32,
}

/// One window's averages.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Average {
    pub window_s: u32,
    pub samples: u32,
    /// Mean of the kernel's 1-minute load over the window.
    pub load1: Option<f64>,
    /// Mean percent of memory free (Linux: available; macOS: `kern.memorystatus_level`).
    pub free_percent: Option<f64>,
    /// The worst memory pressure level seen in the window.
    pub worst_level: Level,
    pub thermal_samples: ThermalTime,
    /// Throttle events the kernel counted during the window, where it counts them.
    pub throttle_events: Option<u64>,
    /// Samples taken on battery.
    pub battery_samples: u32,
}

/// Where the load is heading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Trend {
    Rising,
    Steady,
    Falling,
    /// Not enough history yet.
    Unknown,
}

impl Default for Ring {
    fn default() -> Self {
        Ring::new(CAPACITY)
    }
}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        Ring { cap: cap.max(1), points: VecDeque::new() }
    }

    pub fn push(&mut self, p: Point) {
        if self.points.len() == self.cap {
            self.points.pop_front();
        }
        self.points.push_back(p);
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Seconds from the oldest point to the newest.
    pub fn span_s(&self) -> f64 {
        match (self.points.front(), self.points.back()) {
            (Some(a), Some(b)) => b.t - a.t,
            _ => 0.0,
        }
    }

    /// The points within `window_s` of the newest.
    fn window(&self, window_s: u32) -> impl Iterator<Item = &Point> {
        let from = self.points.back().map_or(0.0, |p| p.t) - f64::from(window_s);
        self.points.iter().filter(move |p| p.t > from)
    }

    pub fn average(&self, window_s: u32) -> Average {
        let pts: Vec<&Point> = self.window(window_s).collect();
        let mean = |f: &dyn Fn(&Point) -> Option<f64>| {
            let v: Vec<f64> = pts.iter().filter_map(|p| f(p)).collect();
            (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
        };
        let mut time = ThermalTime::default();
        for p in &pts {
            match p.thermal {
                Some(ThermalState::Nominal) => time.nominal += 1,
                Some(ThermalState::Fair) => time.fair += 1,
                Some(ThermalState::Serious) => time.serious += 1,
                Some(ThermalState::Critical) => time.critical += 1,
                None => time.unknown += 1,
            }
        }
        let totals: Vec<u64> = pts.iter().filter_map(|p| p.throttle_total).collect();
        Average {
            window_s,
            samples: pts.len() as u32,
            load1: mean(&|p| p.load1),
            free_percent: mean(&|p| p.free_percent),
            worst_level: pts.iter().map(|p| p.level).max().unwrap_or(Level::Normal),
            thermal_samples: time,
            throttle_events: match (totals.first(), totals.last()) {
                (Some(a), Some(b)) => Some(b.saturating_sub(*a)),
                _ => None,
            },
            battery_samples: pts.iter().filter(|p| p.on_battery == Some(true)).count() as u32,
        }
    }

    /// The 1, 5 and 15-minute averages.
    pub fn averages(&self) -> Vec<Average> {
        WINDOWS_S.iter().map(|w| self.average(*w)).collect()
    }

    /// Mean 1-minute load over the last minute against the four minutes before it: rising or
    /// falling when it moved by more than `max(0.1, 5% of the CPU count)`. Needs at least 30
    /// points in each span.
    pub fn load_trend(&self, cpus: usize) -> Trend {
        let Some(now) = self.points.back().map(|p| p.t) else { return Trend::Unknown };
        let span = |from: f64, to: f64| -> Vec<f64> {
            self.points.iter().filter(|p| p.t > now - to && p.t <= now - from).filter_map(|p| p.load1).collect()
        };
        let (recent, before) = (span(0.0, 60.0), span(60.0, 300.0));
        if recent.len() < 30 || before.len() < 30 {
            return Trend::Unknown;
        }
        let m = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let delta = m(&recent) - m(&before);
        let eps = (0.05 * cpus as f64).max(0.1);
        if delta > eps {
            Trend::Rising
        } else if delta < -eps {
            Trend::Falling
        } else {
            Trend::Steady
        }
    }

    /// Throttle events counted in the last `window_s` (`None` when the kernel has no counter).
    pub fn throttle_events(&self, window_s: u32) -> Option<u64> {
        self.average(window_s).throttle_events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(t: f64, load1: f64) -> Point {
        Point {
            t,
            load1: Some(load1),
            free_percent: Some(50.0),
            level: Level::Normal,
            thermal: Some(ThermalState::Nominal),
            throttle_total: None,
            on_battery: None,
        }
    }

    #[test]
    fn keeps_the_newest_capacity_points() {
        let mut r = Ring::new(900);
        for i in 0..1000 {
            r.push(pt(f64::from(i), 1.0));
        }
        assert_eq!(r.len(), 900);
        assert_eq!(r.average(900).samples, 900);
        assert_eq!(r.average(60).samples, 60);
        assert_eq!(Ring::default().len(), 0);
    }

    #[test]
    fn windows_average_the_points_inside_them() {
        let mut r = Ring::default();
        // 600 s at load 1, then 60 s at load 4.
        for i in 0..660 {
            r.push(pt(f64::from(i), 1.0));
        }
        for i in 660..720 {
            r.push(pt(f64::from(i), 4.0));
        }
        let a = r.averages();
        assert_eq!(a.iter().map(|x| x.window_s).collect::<Vec<_>>(), [60, 300, 900]);
        assert_eq!(a[0].load1, Some(4.0));
        assert_eq!(a[1].samples, 300);
        let five = a[1].load1.expect("5 min");
        assert!((five - (240.0 * 1.0 + 60.0 * 4.0) / 300.0).abs() < 1e-9, "{five}");
        assert_eq!(a[2].samples, 720, "everything fits in 15 minutes");
        assert_eq!(Ring::default().average(60).load1, None);
    }

    #[test]
    fn worst_level_free_percent_thermal_time_and_battery() {
        let mut r = Ring::default();
        let levels = [
            (Level::Normal, ThermalState::Nominal, false),
            (Level::Warning, ThermalState::Serious, true),
            (Level::Normal, ThermalState::Serious, true),
            (Level::Normal, ThermalState::Critical, false),
        ];
        for (i, (l, th, bat)) in levels.into_iter().enumerate() {
            r.push(Point {
                level: l,
                thermal: Some(th),
                on_battery: Some(bat),
                free_percent: Some(10.0 * (i + 1) as f64),
                ..pt(i as f64, 1.0)
            });
        }
        r.push(Point { thermal: None, ..pt(4.0, 1.0) });
        let a = r.average(60);
        assert_eq!(a.worst_level, Level::Warning);
        assert_eq!(a.thermal_samples, ThermalTime { nominal: 1, fair: 0, serious: 2, critical: 1, unknown: 1 });
        assert_eq!(a.battery_samples, 2);
        assert_eq!(a.free_percent, Some((10.0 + 20.0 + 30.0 + 40.0 + 50.0) / 5.0));
    }

    #[test]
    fn throttle_events_are_the_counter_delta() {
        let mut r = Ring::default();
        assert_eq!(r.throttle_events(60), None);
        for (i, total) in [5u64, 5, 9, 12].into_iter().enumerate() {
            r.push(Point { throttle_total: Some(total), ..pt(i as f64, 1.0) });
        }
        assert_eq!(r.throttle_events(60), Some(7));
    }

    #[test]
    fn trend_needs_history_and_a_real_move() {
        let build = |recent: f64, before: f64| {
            let mut r = Ring::default();
            for i in 0..300 {
                r.push(pt(f64::from(i), if i >= 240 { recent } else { before }));
            }
            r
        };
        assert_eq!(build(3.0, 1.0).load_trend(4), Trend::Rising);
        assert_eq!(build(1.0, 3.0).load_trend(4), Trend::Falling);
        assert_eq!(build(1.1, 1.0).load_trend(4), Trend::Steady);
        let mut short = Ring::default();
        for i in 0..40 {
            short.push(pt(f64::from(i), 1.0));
        }
        assert_eq!((short.load_trend(4), Ring::default().load_trend(4)), (Trend::Unknown, Trend::Unknown));
    }
}

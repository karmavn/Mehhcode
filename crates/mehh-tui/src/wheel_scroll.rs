//! Mouse wheel scroll acceleration ported from TypeScript `packages/tui/src/wheel-scroll.ts`.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WheelScrollLines {
    Auto,
    Fixed(usize),
}

const BURST_GAP_MS: f64 = 5.0;
const GESTURE_GAP_MS: f64 = 200.0;
const REFERENCE_GAP_MS: f64 = 100.0;
const MAX_AUTO_LINES: usize = 6;

fn terminal_accelerates_wheel() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::env::var("SSH_CONNECTION").is_err()
            && std::env::var("SSH_CLIENT").is_err()
            && std::env::var("SSH_TTY").is_err()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Converts wheel events into line counts.
///
/// In `Auto` mode on terminals that do not accelerate wheel input, the count follows
/// event velocity: an isolated notch moves one line, while a fast spin moves up to six lines.
#[derive(Debug, Clone)]
pub struct WheelScrollAccelerator {
    pub lines: WheelScrollLines,
    pub accelerate: bool,
    last_time: f64,
    last_direction: i8,
    average_gap: Option<f64>,
    carry: f64,
}

impl Default for WheelScrollAccelerator {
    fn default() -> Self {
        Self::new(WheelScrollLines::Auto, !terminal_accelerates_wheel())
    }
}

impl WheelScrollAccelerator {
    pub fn new(lines: WheelScrollLines, accelerate: bool) -> Self {
        Self {
            lines,
            accelerate,
            last_time: f64::NEG_INFINITY,
            last_direction: 0,
            average_gap: None,
            carry: 0.0,
        }
    }

    pub fn set_lines(&mut self, lines: WheelScrollLines) {
        self.lines = lines;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.last_time = f64::NEG_INFINITY;
        self.last_direction = 0;
        self.average_gap = None;
        self.carry = 0.0;
    }

    /// Return the positive line count for a wheel event in `direction` (-1 or 1) at time `now` in ms.
    pub fn next(&mut self, direction: i8, now: f64) -> usize {
        match self.lines {
            WheelScrollLines::Fixed(n) => return n.max(1),
            WheelScrollLines::Auto => {}
        }

        if !self.accelerate {
            return 1;
        }

        let gap = now - self.last_time;
        let same_gesture = direction == self.last_direction && gap <= GESTURE_GAP_MS;
        self.last_time = now;
        self.last_direction = direction;

        if !same_gesture {
            self.average_gap = None;
            self.carry = 0.0;
            return 1;
        }

        if gap < BURST_GAP_MS {
            return 1;
        }

        let avg = match self.average_gap {
            None => gap,
            Some(prev) => (prev + gap) / 2.0,
        };
        self.average_gap = Some(avg);

        let lines = (REFERENCE_GAP_MS / avg).clamp(1.0, MAX_AUTO_LINES as f64) + self.carry;
        let whole = lines.floor();
        self.carry = lines - whole;
        whole as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scroll(accelerator: &mut WheelScrollAccelerator, times: &[f64], direction: i8) -> Vec<usize> {
        times.iter().map(|&t| accelerator.next(direction, t)).collect()
    }

    #[test]
    fn test_fixed_line_counts() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Fixed(3), true);
        assert_eq!(scroll(&mut acc, &[0.0, 10.0, 20.0, 1000.0], 1), vec![3, 3, 3, 3]);
        acc.set_lines(WheelScrollLines::Fixed(1));
        assert_eq!(acc.next(1, 2000.0), 1);
    }

    #[test]
    fn test_auto_mode_no_accelerate() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, false);
        assert_eq!(scroll(&mut acc, &[0.0, 10.0, 20.0, 30.0], 1), vec![1, 1, 1, 1]);
    }

    #[test]
    fn test_scales_with_velocity() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(scroll(&mut acc, &[0.0, 150.0, 300.0, 450.0], 1), vec![1, 1, 1, 1]);
        assert_eq!(scroll(&mut acc, &[1000.0, 1050.0, 1100.0, 1150.0], 1), vec![1, 2, 2, 2]);
        assert_eq!(scroll(&mut acc, &[2000.0, 2020.0, 2040.0, 2060.0], 1), vec![1, 5, 5, 5]);
        assert_eq!(scroll(&mut acc, &[3000.0, 3010.0, 3020.0, 3030.0], 1), vec![1, 6, 6, 6]);
    }

    #[test]
    fn test_burst_not_accelerated() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(scroll(&mut acc, &[0.0, 3.0, 6.0, 9.0], 1), vec![1, 1, 1, 1]);
    }

    #[test]
    fn test_resets_on_direction_change_and_pause() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(scroll(&mut acc, &[0.0, 20.0, 40.0], 1), vec![1, 5, 5]);
        assert_eq!(acc.next(-1, 60.0), 1);
        assert_eq!(scroll(&mut acc, &[500.0, 520.0], 1), vec![1, 5]);
    }

    #[test]
    fn test_fractional_line_carry() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(scroll(&mut acc, &[0.0, 40.0, 80.0, 120.0, 160.0], 1), vec![1, 2, 3, 2, 3]);
    }
}

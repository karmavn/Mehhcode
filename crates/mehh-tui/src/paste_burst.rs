use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const BURST_INTERVAL: Duration = Duration::from_millis(15);
const PENDING_CHAR_FLUSH: Duration = Duration::from_millis(10);
const PENDING_ENTER_FLUSH: Duration = Duration::from_millis(25);
const ACTIVE_IDLE_FLUSH: Duration = Duration::from_millis(50);
const ACTIVE_CHAR_THRESHOLD: usize = 8;
const TRAILING_ENTER_MIN_CHARS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteKey {
    Char(char),
    Tab,
    Enter,
}

#[derive(Debug)]
enum BurstState {
    Idle,
    Pending {
        events: Vec<KeyEvent>,
        last_at: Instant,
        saw_enter: bool,
    },
    Active {
        buffer: String,
        last_at: Instant,
    },
}

fn classify_paste_key(key: &KeyEvent, in_active: bool) -> Option<PasteKey> {
    let mods = key.modifiers;
    let has_ctrl = mods.contains(KeyModifiers::CONTROL);
    let has_alt = mods.contains(KeyModifiers::ALT);
    let has_super = mods.contains(KeyModifiers::SUPER);

    match key.code {
        KeyCode::Char('\r' | '\n') if mods.is_empty() => Some(PasteKey::Enter),
        KeyCode::Char(c) if !c.is_control() => {
            let is_windows_altgr = cfg!(windows)
                && has_ctrl
                && has_alt
                && !has_super
                && !c.is_ascii_alphabetic()
                && c != ' ';
            if is_windows_altgr || (!has_ctrl && !has_alt && !has_super) {
                Some(PasteKey::Char(c))
            } else {
                None
            }
        }
        KeyCode::Tab if mods.is_empty() => Some(PasteKey::Tab),
        KeyCode::Enter if mods.is_empty() => Some(PasteKey::Enter),
        KeyCode::Enter
            if in_active
                && !has_alt
                && !has_super
                && (mods == KeyModifiers::SHIFT || mods == KeyModifiers::CONTROL) =>
        {
            Some(PasteKey::Enter)
        }
        _ => None,
    }
}

fn push_paste_key(buf: &mut String, pk: PasteKey) {
    match pk {
        PasteKey::Char(c) => buf.push(c),
        PasteKey::Tab => buf.push('\t'),
        PasteKey::Enter => buf.push('\n'),
    }
}

fn pending_events_to_string(events: &[KeyEvent]) -> String {
    let mut buf = String::with_capacity(events.len());
    for ev in events {
        if let Some(pk) = classify_paste_key(ev, false) {
            push_paste_key(&mut buf, pk);
        }
    }
    buf
}

/// Coalesces rapid `Event::Key` bursts (e.g., Windows ConPTY clipboard pastes)
/// into a single `Event::Paste` so embedded newlines do not trigger submission.
#[derive(Debug)]
pub struct PasteBurst {
    state: BurstState,
}

impl Default for PasteBurst {
    fn default() -> Self {
        Self::new()
    }
}

impl PasteBurst {
    pub fn new() -> Self {
        Self {
            state: BurstState::Idle,
        }
    }

    pub fn next_flush_deadline(&self) -> Option<Instant> {
        match &self.state {
            BurstState::Idle => None,
            BurstState::Pending {
                last_at, saw_enter, ..
            } => {
                let delay = if *saw_enter {
                    PENDING_ENTER_FLUSH
                } else {
                    PENDING_CHAR_FLUSH
                };
                Some(*last_at + delay)
            }
            BurstState::Active { last_at, .. } => Some(*last_at + ACTIVE_IDLE_FLUSH),
        }
    }

    pub fn flush_if_due(&mut self, now: Instant) -> Vec<Event> {
        if self.next_flush_deadline().is_some_and(|d| now >= d) {
            self.flush_now()
        } else {
            Vec::new()
        }
    }

    pub fn flush_now(&mut self) -> Vec<Event> {
        match std::mem::replace(&mut self.state, BurstState::Idle) {
            BurstState::Idle => Vec::new(),
            BurstState::Pending { events, .. } => events.into_iter().map(Event::Key).collect(),
            BurstState::Active { buffer, .. } => {
                if buffer.is_empty() {
                    Vec::new()
                } else {
                    vec![Event::Paste(buffer)]
                }
            }
        }
    }

    pub fn on_event(&mut self, event: Event, now: Instant) -> Vec<Event> {
        let mut out = self.flush_if_due(now);

        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Release {
                return out;
            }
        }

        let key = match event {
            Event::Key(key) => key,
            other => {
                out.extend(self.flush_now());
                out.push(other);
                return out;
            }
        };

        let prev_state = std::mem::replace(&mut self.state, BurstState::Idle);
        match prev_state {
            BurstState::Active {
                mut buffer,
                last_at: _,
            } => {
                if let Some(pk) = classify_paste_key(&key, true) {
                    push_paste_key(&mut buffer, pk);
                    self.state = BurstState::Active {
                        buffer,
                        last_at: now,
                    };
                } else {
                    self.state = BurstState::Active {
                        buffer,
                        last_at: now,
                    };
                    out.extend(self.flush_now());
                    out.push(Event::Key(key));
                }
                out
            }
            BurstState::Pending {
                mut events,
                last_at,
                saw_enter,
            } => {
                let elapsed = now.saturating_duration_since(last_at);
                let within_burst = if saw_enter {
                    elapsed <= PENDING_ENTER_FLUSH
                } else {
                    elapsed <= BURST_INTERVAL
                };

                if !within_burst {
                    out.extend(events.into_iter().map(Event::Key));
                    self.handle_idle_key(key, now, &mut out);
                    return out;
                }

                match classify_paste_key(&key, false) {
                    Some(pk) if saw_enter => {
                        let mut buffer = pending_events_to_string(&events);
                        push_paste_key(&mut buffer, pk);
                        self.state = BurstState::Active {
                            buffer,
                            last_at: now,
                        };
                    }
                    Some(PasteKey::Enter) => {
                        if events.len() >= TRAILING_ENTER_MIN_CHARS {
                            let mut buffer = pending_events_to_string(&events);
                            buffer.push('\n');
                            self.state = BurstState::Active {
                                buffer,
                                last_at: now,
                            };
                        } else {
                            events.push(key);
                            self.state = BurstState::Pending {
                                events,
                                last_at: now,
                                saw_enter: true,
                            };
                        }
                    }
                    Some(PasteKey::Char(_)) | Some(PasteKey::Tab) => {
                        events.push(key);
                        if events.len() >= ACTIVE_CHAR_THRESHOLD {
                            let buffer = pending_events_to_string(&events);
                            self.state = BurstState::Active {
                                buffer,
                                last_at: now,
                            };
                        } else {
                            self.state = BurstState::Pending {
                                events,
                                last_at: now,
                                saw_enter: false,
                            };
                        }
                    }
                    None => {
                        out.extend(events.into_iter().map(Event::Key));
                        out.push(Event::Key(key));
                    }
                }
                out
            }
            BurstState::Idle => {
                self.handle_idle_key(key, now, &mut out);
                out
            }
        }
    }

    fn handle_idle_key(&mut self, key: KeyEvent, now: Instant, out: &mut Vec<Event>) {
        match classify_paste_key(&key, false) {
            Some(PasteKey::Char(_)) => {
                self.state = BurstState::Pending {
                    events: vec![key],
                    last_at: now,
                    saw_enter: false,
                };
            }
            Some(PasteKey::Enter) => {
                self.state = BurstState::Pending {
                    events: vec![key],
                    last_at: now,
                    saw_enter: true,
                };
            }
            Some(PasteKey::Tab) | None => {
                out.push(Event::Key(key));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Component;
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};

    fn key_press(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Press))
    }

    fn key_release(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Release))
    }

    fn char_press(c: char) -> Event {
        let mods = if c.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        key_press(KeyCode::Char(c), mods)
    }

    fn char_release(c: char) -> Event {
        let mods = if c.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        key_release(KeyCode::Char(c), mods)
    }

    fn feed_text_burst(
        pb: &mut PasteBurst,
        text: &str,
        start: Instant,
        step: Duration,
        with_releases: bool,
        out: &mut Vec<Event>,
    ) -> Instant {
        let mut t = start;
        for ch in text.chars() {
            let (code, mods) = match ch {
                '\n' => (KeyCode::Enter, KeyModifiers::NONE),
                '\t' => (KeyCode::Tab, KeyModifiers::NONE),
                c if c.is_ascii_uppercase() => (KeyCode::Char(c), KeyModifiers::SHIFT),
                c => (KeyCode::Char(c), KeyModifiers::NONE),
            };
            out.extend(pb.on_event(key_press(code, mods), t));
            if with_releases {
                out.extend(pb.on_event(key_release(code, mods), t));
            }
            t += step;
        }
        t
    }

    #[test]
    fn multiline_windows_paste_with_interleaved_releases_coalesces_without_submit() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        let mut emitted = Vec::new();
        let t_end = feed_text_burst(
            &mut pb,
            "line 1\nLine 2\nline 3",
            t0,
            Duration::from_millis(1),
            true,
            &mut emitted,
        );
        assert!(
            !emitted.iter().any(|e| matches!(e, Event::Key(k) if k.code == KeyCode::Enter)),
            "must not emit KeyCode::Enter mid-paste: {emitted:?}"
        );

        let deadline = pb.next_flush_deadline().expect("active paste must have deadline");
        emitted.extend(pb.flush_if_due(deadline.max(t_end + Duration::from_millis(60))));

        assert_eq!(
            emitted,
            vec![Event::Paste("line 1\nLine 2\nline 3".to_string())]
        );
    }

    #[test]
    fn large_paste_with_mid_burst_conpty_pause_stays_single_paste() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        let mut emitted = Vec::new();

        let first_half: String = (1..=8).map(|i| format!("line {i}\n")).collect();
        let second_half: String =
            (9..=14).map(|i| format!("line {i}\n")).collect::<String>() + "line 15";
        let full_prompt = format!("{first_half}{second_half}");

        let t1 = feed_text_burst(
            &mut pb,
            &first_half,
            t0,
            Duration::from_millis(1),
            false,
            &mut emitted,
        );
        // Simulate a 35 ms ConPTY chunk pause (< ACTIVE_IDLE_FLUSH 50 ms)
        let t2 = t1 + Duration::from_millis(35);
        assert!(pb.flush_if_due(t2).is_empty());

        let t3 = feed_text_burst(
            &mut pb,
            &second_half,
            t2,
            Duration::from_millis(1),
            false,
            &mut emitted,
        );
        emitted.extend(pb.flush_if_due(t3 + Duration::from_millis(60)));

        assert_eq!(emitted, vec![Event::Paste(full_prompt.clone())]);

        let mut editor = crate::Editor::new();
        if let Event::Paste(pasted) = &emitted[0] {
            editor.insert_text(pasted);
        }
        let rendered = editor.render(80).join("\n");
        assert!(
            rendered.contains("[paste #1 +15 lines]"),
            "expected collapsed paste chip in editor render:\n{rendered}"
        );
        assert_eq!(editor.text(), full_prompt);
    }

    #[test]
    fn short_first_line_and_leading_newline_pastes_do_not_submit() {
        for sample in ["a\nb", "Hi\nline 2", "\nline 2", "\n\nhello"] {
            let mut pb = PasteBurst::new();
            let t0 = Instant::now();
            let mut emitted = Vec::new();
            let t_end = feed_text_burst(
                &mut pb,
                sample,
                t0,
                Duration::from_millis(1),
                false,
                &mut emitted,
            );
            emitted.extend(pb.flush_if_due(t_end + Duration::from_millis(60)));
            assert_eq!(
                emitted,
                vec![Event::Paste(sample.to_string())],
                "failed for sample {sample:?}"
            );
        }
    }

    #[test]
    fn single_line_paste_with_trailing_newline_does_not_submit() {
        for sample in ["hello\n", "fix the bug in auth.rs\n"] {
            let mut pb = PasteBurst::new();
            let t0 = Instant::now();
            let mut emitted = Vec::new();
            let t_end = feed_text_burst(
                &mut pb,
                sample,
                t0,
                Duration::from_millis(1),
                false,
                &mut emitted,
            );
            emitted.extend(pb.flush_if_due(t_end + Duration::from_millis(60)));
            assert_eq!(
                emitted,
                vec![Event::Paste(sample.to_string())],
                "failed for sample {sample:?}"
            );
            assert!(
                !emitted.iter().any(|e| matches!(e, Event::Key(k) if k.code == KeyCode::Enter)),
                "must not emit KeyCode::Enter for {sample:?}"
            );
        }
    }

    #[test]
    fn normal_and_fast_human_typing_preserves_key_events_and_submits() {
        // Normal typing at 80 ms spacing
        let mut pb = PasteBurst::new();
        let mut t = Instant::now();
        let mut emitted = Vec::new();
        for ch in "hello".chars() {
            emitted.extend(pb.on_event(char_press(ch), t));
            t += Duration::from_millis(80);
            emitted.extend(pb.flush_if_due(t));
        }
        emitted.extend(pb.on_event(key_press(KeyCode::Enter, KeyModifiers::NONE), t));
        t += Duration::from_millis(30);
        emitted.extend(pb.flush_if_due(t));
        assert_eq!(
            emitted,
            vec![
                char_press('h'),
                char_press('e'),
                char_press('l'),
                char_press('l'),
                char_press('o'),
                key_press(KeyCode::Enter, KeyModifiers::NONE),
            ]
        );

        // Ultra-fast 2-finger 'y' + Enter (5 ms apart)
        let mut pb_fast_y = PasteBurst::new();
        let t0 = Instant::now();
        let mut out_y = Vec::new();
        out_y.extend(pb_fast_y.on_event(char_press('y'), t0));
        out_y.extend(pb_fast_y.on_event(
            key_press(KeyCode::Enter, KeyModifiers::NONE),
            t0 + Duration::from_millis(5),
        ));
        out_y.extend(pb_fast_y.flush_if_due(t0 + Duration::from_millis(35)));
        assert_eq!(
            out_y,
            vec![
                char_press('y'),
                key_press(KeyCode::Enter, KeyModifiers::NONE),
            ]
        );

        // Ultra-fast "ok" + Enter (5 ms apart)
        let mut pb_fast_ok = PasteBurst::new();
        let t1 = Instant::now();
        let mut out_ok = Vec::new();
        out_ok.extend(pb_fast_ok.on_event(char_press('o'), t1));
        out_ok.extend(pb_fast_ok.on_event(char_press('k'), t1 + Duration::from_millis(5)));
        out_ok.extend(pb_fast_ok.on_event(
            key_press(KeyCode::Enter, KeyModifiers::NONE),
            t1 + Duration::from_millis(10),
        ));
        out_ok.extend(pb_fast_ok.flush_if_due(t1 + Duration::from_millis(40)));
        assert_eq!(
            out_ok,
            vec![
                char_press('o'),
                char_press('k'),
                key_press(KeyCode::Enter, KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn vietnamese_ime_short_burst_before_enter_preserves_key_events() {
        let mut pb = PasteBurst::new();
        let mut t = Instant::now();
        let mut emitted = Vec::new();

        for _ in 0..3 {
            emitted.extend(pb.on_event(key_press(KeyCode::Backspace, KeyModifiers::NONE), t));
            t += Duration::from_millis(1);
        }
        for ch in ['ế', 'n', 'g'] {
            emitted.extend(pb.on_event(key_press(KeyCode::Char(ch), KeyModifiers::NONE), t));
            t += Duration::from_millis(1);
        }
        // User pauses 50 ms then presses Enter
        t += Duration::from_millis(50);
        emitted.extend(pb.on_event(key_press(KeyCode::Enter, KeyModifiers::NONE), t));
        t += Duration::from_millis(30);
        emitted.extend(pb.flush_if_due(t));

        assert_eq!(
            emitted,
            vec![
                key_press(KeyCode::Backspace, KeyModifiers::NONE),
                key_press(KeyCode::Backspace, KeyModifiers::NONE),
                key_press(KeyCode::Backspace, KeyModifiers::NONE),
                key_press(KeyCode::Char('ế'), KeyModifiers::NONE),
                key_press(KeyCode::Char('n'), KeyModifiers::NONE),
                key_press(KeyCode::Char('g'), KeyModifiers::NONE),
                key_press(KeyCode::Enter, KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn tab_and_shift_enter_in_idle_pass_through_immediately() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();

        let tab_out = pb.on_event(key_press(KeyCode::Tab, KeyModifiers::NONE), t0);
        assert_eq!(tab_out, vec![key_press(KeyCode::Tab, KeyModifiers::NONE)]);
        assert_eq!(pb.next_flush_deadline(), None);

        let shift_enter_out = pb.on_event(key_press(KeyCode::Enter, KeyModifiers::SHIFT), t0);
        assert_eq!(
            shift_enter_out,
            vec![key_press(KeyCode::Enter, KeyModifiers::SHIFT)]
        );
        assert_eq!(pb.next_flush_deadline(), None);

        // Tab inside an Active paste becomes '\t'
        let mut emitted = Vec::new();
        let t_end = feed_text_burst(
            &mut pb,
            "col1_value\tcol2_value",
            t0 + Duration::from_millis(100),
            Duration::from_millis(1),
            false,
            &mut emitted,
        );
        emitted.extend(pb.flush_if_due(t_end + Duration::from_millis(60)));
        assert_eq!(emitted, vec![Event::Paste("col1_value\tcol2_value".to_string())]);
    }

    #[test]
    fn non_paste_event_and_ctrl_c_flush_active_paste_first() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        let mut emitted = Vec::new();

        let t1 = feed_text_burst(
            &mut pb,
            "long pasted text ",
            t0,
            Duration::from_millis(1),
            false,
            &mut emitted,
        );
        let ctrl_c = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        emitted.extend(pb.on_event(ctrl_c.clone(), t1));
        assert_eq!(
            emitted,
            vec![Event::Paste("long pasted text ".to_string()), ctrl_c]
        );
        assert_eq!(pb.next_flush_deadline(), None);

        // Mouse event during Pending flushes pending keys first
        let mut t2 = t1 + Duration::from_millis(100);
        let mut out2 = pb.on_event(char_press('a'), t2);
        t2 += Duration::from_millis(2);
        let mouse = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        out2.extend(pb.on_event(mouse.clone(), t2));
        assert_eq!(out2, vec![char_press('a'), mouse]);

        // Native Event::Paste during Active flushes active paste first
        let mut out3 = Vec::new();
        let t3 = feed_text_burst(
            &mut pb,
            "prefix pasted ",
            t2 + Duration::from_millis(20),
            Duration::from_millis(1),
            false,
            &mut out3,
        );
        out3.extend(pb.on_event(Event::Paste("native".to_string()), t3));
        assert_eq!(
            out3,
            vec![
                Event::Paste("prefix pasted ".to_string()),
                Event::Paste("native".to_string())
            ]
        );
    }

    #[test]
    fn ctrl_enter_inside_active_paste_becomes_newline() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        let mut emitted = Vec::new();

        let t1 = feed_text_burst(
            &mut pb,
            "first pasted line",
            t0,
            Duration::from_millis(1),
            false,
            &mut emitted,
        );
        emitted.extend(pb.on_event(key_press(KeyCode::Enter, KeyModifiers::CONTROL), t1));
        let t2 = feed_text_burst(
            &mut pb,
            "second line",
            t1 + Duration::from_millis(1),
            Duration::from_millis(1),
            false,
            &mut emitted,
        );
        emitted.extend(pb.flush_if_due(t2 + Duration::from_millis(60)));

        assert_eq!(
            emitted,
            vec![Event::Paste("first pasted line\nsecond line".to_string())]
        );
    }

    #[test]
    fn release_after_deadline_flushes_expired_pending_or_active() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        assert!(pb.on_event(char_press('x'), t0).is_empty());
        let deadline = pb.next_flush_deadline().expect("pending deadline expected");

        // Release at/after deadline must flush pending 'x' and drop Release
        let out = pb.on_event(char_release('x'), deadline);
        assert_eq!(out, vec![char_press('x')]);
        assert_eq!(pb.next_flush_deadline(), None);

        // Same for Active
        let mut out2 = Vec::new();
        let t1 = feed_text_burst(
            &mut pb,
            "abcdefghij",
            deadline + Duration::from_millis(20),
            Duration::from_millis(1),
            false,
            &mut out2,
        );
        let active_deadline = pb.next_flush_deadline().expect("active deadline expected");
        assert!(active_deadline > t1);
        let flushed = pb.on_event(char_release('j'), active_deadline);
        assert_eq!(flushed, vec![Event::Paste("abcdefghij".to_string())]);
        assert_eq!(pb.next_flush_deadline(), None);
    }

    #[cfg(windows)]
    #[test]
    fn altgr_chars_inside_windows_paste() {
        let mut pb = PasteBurst::new();
        let t0 = Instant::now();
        let mut emitted = Vec::new();
        let t1 = feed_text_burst(
            &mut pb,
            "useremail",
            t0,
            Duration::from_millis(1),
            false,
            &mut emitted,
        );
        emitted.extend(pb.on_event(
            key_press(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            t1,
        ));
        let t2 = feed_text_burst(
            &mut pb,
            "example.com",
            t1 + Duration::from_millis(1),
            Duration::from_millis(1),
            false,
            &mut emitted,
        );
        emitted.extend(pb.flush_if_due(t2 + Duration::from_millis(60)));
        assert_eq!(
            emitted,
            vec![Event::Paste("useremail@example.com".to_string())]
        );
    }
}

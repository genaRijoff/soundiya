//! Messages for the Telegram bot: "Алиса, передай Маше, что я буду через час".
//! Alice's recognized phrase waits here until the bot collects it with `MESSAGES`. Only while
//! the bot keeps collecting (a lease renewed by each `MESSAGES`) does soundiya take such
//! phrases from Alice; with no bot listening she answers them herself as before.
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

/// How long one `MESSAGES` keeps the phrases coming here.
const LEASE: Duration = Duration::from_secs(30);
/// Phrases not collected yet; older ones are dropped beyond this.
const MAX_QUEUE: usize = 8;
/// Longer phrases are cut (Alice's recognition rarely gets near it).
const MAX_CHARS: usize = 500;

#[derive(Default)]
pub struct Mailbox {
    inner: Mutex<Inner>,
}
#[derive(Default)]
struct Inner {
    lease_until: Option<Instant>,
    queue: VecDeque<String>,
}

impl Mailbox {
    fn active(inner: &Inner, now: Instant) -> bool {
        inner.lease_until.is_some_and(|until| now < until)
    }

    /// Keeps the phrase for the bot; false (and nothing kept) when no bot is collecting.
    pub fn push(&self, text: &str) -> bool {
        self.push_at(text, Instant::now())
    }
    fn push_at(&self, text: &str, now: Instant) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !Self::active(&inner, now) {
            return false;
        }
        if inner.queue.len() == MAX_QUEUE {
            inner.queue.pop_front();
        }
        inner
            .queue
            .push_back(text.chars().take(MAX_CHARS).collect());
        true
    }

    /// `MESSAGES`: renews the lease and hands over what waits, as a JSON array of strings.
    pub fn collect(&self) -> String {
        self.collect_at(Instant::now())
    }
    fn collect_at(&self, now: Instant) -> String {
        let mut inner = self.inner.lock().unwrap();
        inner.lease_until = Some(now + LEASE);
        let texts: Vec<String> = inner.queue.drain(..).collect();
        serde_json::to_string(&texts).unwrap_or_else(|_| "[]".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_are_kept_only_while_the_bot_collects() {
        let mailbox = Mailbox::default();
        let start = Instant::now();
        // No bot yet: Alice keeps the phrase.
        assert!(!mailbox.push_at("Маше привет", start));
        assert_eq!(mailbox.collect_at(start), "[]");
        // The bot asked: phrases wait for it, in order, once.
        assert!(mailbox.push_at("Маше привет", start + Duration::from_secs(1)));
        assert!(mailbox.push_at("маме \"пока\"", start + Duration::from_secs(2)));
        assert_eq!(
            mailbox.collect_at(start + Duration::from_secs(3)),
            r#"["Маше привет","маме \"пока\""]"#
        );
        assert_eq!(mailbox.collect_at(start + Duration::from_secs(4)), "[]");
        // The bot stopped asking: after the lease Alice gets them back.
        assert!(!mailbox.push_at("Маше привет", start + Duration::from_secs(4) + LEASE));
    }

    #[test]
    fn the_queue_and_the_text_are_bounded() {
        let mailbox = Mailbox::default();
        let now = Instant::now();
        mailbox.collect_at(now);
        for n in 0..20 {
            mailbox.push_at(&format!("m{n}"), now);
        }
        let texts: Vec<String> = serde_json::from_str(&mailbox.collect_at(now)).unwrap();
        assert_eq!(texts.len(), MAX_QUEUE);
        assert_eq!(texts[0], "m12");
        mailbox.push_at(&"я".repeat(2000), now);
        let texts: Vec<String> = serde_json::from_str(&mailbox.collect_at(now)).unwrap();
        assert_eq!(texts[0].chars().count(), MAX_CHARS);
    }
}

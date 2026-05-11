#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Mutex;

use roki_api_types::{ApiEvent, EventsPage};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

pub struct EventRing {
    capacity: usize,
    inner: Mutex<RingInner>,
}

struct RingInner {
    next_seq: u64,
    buf: VecDeque<RingEntry>,
}

#[derive(Clone)]
struct RingEntry {
    seq: u64,
    ts: OffsetDateTime,
    event: String,
    ticket_id: Option<String>,
    cycle_id: Option<Uuid>,
    payload: Value,
}

impl EventRing {
    pub fn new(capacity: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            capacity,
            inner: Mutex::new(RingInner {
                next_seq: 1,
                buf: VecDeque::with_capacity(capacity.max(1)),
            }),
        })
    }

    pub fn record(
        &self,
        event: &str,
        ticket_id: Option<&str>,
        cycle_id: Option<Uuid>,
        payload: Value,
    ) -> u64 {
        if self.capacity == 0 {
            // Ring disabled. Still bump seq so callers get a strictly
            // increasing sequence number if they ever need one.
            let mut g = self.inner.lock().expect("ring lock");
            let seq = g.next_seq;
            g.next_seq += 1;
            return seq;
        }
        let mut g = self.inner.lock().expect("ring lock");
        let seq = g.next_seq;
        g.next_seq += 1;
        if g.buf.len() == self.capacity {
            g.buf.pop_front();
        }
        g.buf.push_back(RingEntry {
            seq,
            ts: OffsetDateTime::now_utc(),
            event: event.to_string(),
            ticket_id: ticket_id.map(str::to_string),
            cycle_id,
            payload,
        });
        seq
    }

    pub fn page(
        &self,
        since: Option<u64>,
        kind: Option<&str>,
        ticket: Option<&str>,
        cycle: Option<Uuid>,
        limit: usize,
    ) -> EventsPage {
        let g = self.inner.lock().expect("ring lock");
        let oldest = g.buf.front().map(|e| e.seq);
        let gap = match (since, oldest) {
            (Some(s), Some(o)) => s + 1 < o,
            (Some(_), None) => true,
            _ => false,
        };
        let start_after = since.unwrap_or(0);
        let mut out: Vec<ApiEvent> = Vec::new();
        for e in g.buf.iter() {
            if e.seq <= start_after {
                continue;
            }
            if let Some(k) = kind {
                if e.event != k {
                    continue;
                }
            }
            if let Some(t) = ticket {
                if e.ticket_id.as_deref() != Some(t) {
                    continue;
                }
            }
            if let Some(c) = cycle {
                if e.cycle_id != Some(c) {
                    continue;
                }
            }
            out.push(ApiEvent {
                seq: e.seq,
                ts: e.ts,
                event: e.event.clone(),
                ticket_id: e.ticket_id.clone(),
                cycle_id: e.cycle_id,
                payload: e.payload.clone(),
            });
            if out.len() >= limit {
                break;
            }
        }
        let next_since = out.last().map(|e| e.seq);
        EventsPage {
            events: out,
            gap,
            next_since,
        }
    }

    pub fn oldest_seq(&self) -> Option<u64> {
        self.inner
            .lock()
            .expect("ring lock")
            .buf
            .front()
            .map(|e| e.seq)
    }

    pub fn newest_seq(&self) -> Option<u64> {
        self.inner
            .lock()
            .expect("ring lock")
            .buf
            .back()
            .map(|e| e.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_assigns_monotonic_seq() {
        let r = EventRing::new(10);
        assert_eq!(r.record("a", None, None, Value::Null), 1);
        assert_eq!(r.record("b", None, None, Value::Null), 2);
        assert_eq!(r.record("c", None, None, Value::Null), 3);
    }

    #[test]
    fn page_since_returns_only_newer() {
        let r = EventRing::new(10);
        for i in 0..5 {
            r.record(&format!("e{i}"), None, None, Value::Null);
        }
        let p = r.page(Some(2), None, None, None, 100);
        assert_eq!(p.events.len(), 3);
        assert_eq!(p.events[0].seq, 3);
        assert!(!p.gap);
    }

    #[test]
    fn page_gap_when_since_older_than_oldest() {
        let r = EventRing::new(2);
        for i in 0..5 {
            r.record(&format!("e{i}"), None, None, Value::Null);
        }
        let p = r.page(Some(1), None, None, None, 100);
        assert!(p.gap, "since=1 must report gap when oldest is 4");
    }

    #[test]
    fn kind_filter() {
        let r = EventRing::new(10);
        r.record("a", None, None, Value::Null);
        r.record("b", None, None, Value::Null);
        let p = r.page(None, Some("a"), None, None, 100);
        assert_eq!(p.events.len(), 1);
        assert_eq!(p.events[0].event, "a");
    }

    #[test]
    fn capacity_zero_no_op() {
        let r = EventRing::new(0);
        r.record("a", None, None, Value::Null);
        let p = r.page(None, None, None, None, 100);
        assert!(p.events.is_empty());
        assert!(!p.gap);
    }

    #[test]
    fn ticket_filter_only_returns_matching() {
        let r = EventRing::new(10);
        r.record("e", Some("A"), None, Value::Null);
        r.record("e", Some("B"), None, Value::Null);
        r.record("e", Some("A"), None, Value::Null);
        let p = r.page(None, None, Some("A"), None, 100);
        assert_eq!(p.events.len(), 2);
        assert!(p.events.iter().all(|e| e.ticket_id.as_deref() == Some("A")));
    }

    #[test]
    fn limit_truncates_and_next_since_is_last_seq() {
        let r = EventRing::new(10);
        for i in 0..5 {
            r.record(&format!("e{i}"), None, None, Value::Null);
        }
        let p = r.page(None, None, None, None, 2);
        assert_eq!(p.events.len(), 2);
        assert_eq!(p.next_since, Some(p.events.last().unwrap().seq));
    }
}

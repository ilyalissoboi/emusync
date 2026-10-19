//! The pull-model event queue (DESIGN §3.2): the worker pushes, the host polls on a thread of
//! its choice. A polled event's strings are owned here until `es_event_release`, keyed by a
//! token stored in `es_event._internal`, so a double release or a stale pointer is harmless.

use crate::abi::{self, es_event};
use crate::callbacks::{cstring_lossy, Callbacks};
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::sync::Mutex;

/// Oldest events are dropped beyond this, so a host that never polls can't exhaust memory.
const MAX_QUEUED: usize = 4096;

#[derive(Debug, Clone, Default)]
pub(crate) struct Event {
    pub kind: i32,
    pub status: i32,
    pub container: u64,
    pub conflict_id: u64,
    pub title_key: Option<String>,
    pub unit_id: Option<String>,
    pub unit_name: Option<String>,
    pub device_name: Option<String>,
    pub message: Option<String>,
}

impl Event {
    pub fn new(kind: i32, status: i32) -> Self {
        Self { kind, status, ..Default::default() }
    }

    pub fn message(mut self, m: impl Into<String>) -> Self {
        self.message = Some(m.into());
        self
    }

    /// Sets the unit fields from a unit id (`ps2:Mcd001:BASLUS-20062GTA3`).
    pub fn unit(mut self, unit: &str) -> Self {
        self.unit_name = emusync_core::split_unit(unit).map(|(_, n)| n.to_string());
        self.unit_id = Some(unit.to_string());
        self
    }
}

/// The strings of an event the host is holding.
struct Owned {
    _strings: Vec<CString>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Event>,
    dropped: u64,
    held: HashMap<usize, Owned>,
    next_token: usize,
}

#[derive(Default)]
pub(crate) struct EventQueue {
    state: Mutex<State>,
}

impl EventQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues an event; wakes the host if the queue was empty.
    pub fn push(&self, ev: Event, cb: &Callbacks) {
        let was_empty = {
            let mut st = self.lock();
            let was_empty = st.queue.is_empty();
            if st.queue.len() >= MAX_QUEUED {
                st.queue.pop_front();
                st.dropped += 1;
            }
            st.queue.push_back(ev);
            was_empty
        };
        if was_empty {
            cb.wake();
        }
    }

    /// Dequeues into `out` (whose `struct_size` the caller checked). Returns false if empty.
    pub fn poll(&self, out: &mut es_event) -> bool {
        let mut st = self.lock();
        if st.dropped > 0 {
            let n = std::mem::take(&mut st.dropped);
            let ev = Event::new(abi::ES_EV_ERROR, abi::ES_E_INTERNAL)
                .message(format!("{n} events were dropped because the queue was full"));
            st.queue.push_front(ev);
        }
        let Some(ev) = st.queue.pop_front() else { return false };
        let mut strings = Vec::new();
        let mut keep = |s: Option<String>| match s {
            Some(s) => {
                let c = cstring_lossy(&s);
                // The heap buffer of a CString doesn't move when the CString does.
                let p = c.as_ptr();
                strings.push(c);
                p
            }
            None => std::ptr::null(),
        };
        let title_key = keep(ev.title_key);
        let unit_id = keep(ev.unit_id);
        let unit_name = keep(ev.unit_name);
        let device_name = keep(ev.device_name);
        let message = keep(ev.message);
        st.next_token += 1;
        let token = st.next_token;
        st.held.insert(token, Owned { _strings: strings });
        *out = es_event {
            struct_size: out.struct_size,
            type_: ev.kind,
            status: ev.status,
            container: ev.container,
            conflict_id: ev.conflict_id,
            bytes_done: 0,
            bytes_total: 0,
            title_key,
            unit_id,
            unit_name,
            device_name,
            message,
            _internal: token as *mut std::ffi::c_void,
        };
        true
    }

    /// Frees a polled event's strings and clears its pointers.
    pub fn release(&self, ev: &mut es_event) {
        let token = ev._internal as usize;
        if token != 0 {
            self.lock().held.remove(&token);
        }
        ev.title_key = std::ptr::null();
        ev.unit_id = std::ptr::null();
        ev.unit_name = std::ptr::null();
        ev.device_name = std::ptr::null();
        ev.message = std::ptr::null();
        ev._internal = std::ptr::null_mut();
    }
}

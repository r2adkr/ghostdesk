//! Turns on screens there can only be one of.
//!
//! Most screens are made per agent, but a few exist once: the Windows child session (one per user
//! account) and, on macOS, the separate session of each dedicated account. Two agents driving one
//! of those at the same time mix their clicks and typing, so they take turns: the first one holds
//! the turn until its screen closes, the others wait in line in the order they asked.
//!
//! Agents inside GhostDesk wait as long as it takes (their status shows who they are waiting for).
//! External agents wait a short while only, since their MCP call times out; after that they get
//! who is using it and are told to pick another mode or try again later.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use parking_lot::Mutex;
use tokio::sync::Notify;

#[derive(Default)]
struct Slot {
    owner: Option<(u64, String)>,
    line: VecDeque<(u64, String)>,
}

#[derive(Default)]
pub struct Turns {
    slots: Mutex<HashMap<String, Slot>>,
    changed: Notify,
    next: AtomicU64,
    /// Places in line kept for external agents whose call ran out of time: (key, connection) → place.
    /// The same agent asking again within `PARKED_FOR` continues from the same place.
    parked: Mutex<HashMap<(String, u64), (Turn, Instant)>>,
}

/// How long a kept place in line waits for its agent to ask again.
const PARKED_FOR: Duration = Duration::from_secs(120);

/// Holding the turn on `key`. Dropping it hands the screen to the next in line.
pub struct Turn {
    key: String,
    ticket: u64,
    turns: &'static Turns,
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.turns.release(&self.key, self.ticket);
    }
}

/// What is happening on a key, for status lines: who has it and who waits.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct State {
    pub owner: Option<String>,
    pub waiting: Vec<String>,
}

/// The process-wide turns.
pub fn global() -> &'static Turns {
    static T: OnceLock<Turns> = OnceLock::new();
    T.get_or_init(Turns::default)
}

/// Key of the Windows child session.
pub const CHILD_SESSION: &str = "windows-child-session";

/// Key of a macOS dedicated account's session.
pub fn account_key(account: &str) -> String {
    format!("account:{}", account.to_lowercase())
}

impl Turns {
    fn release(&self, key: &str, ticket: u64) {
        let mut slots = self.slots.lock();
        if let Some(s) = slots.get_mut(key) {
            if s.owner.as_ref().is_some_and(|(t, _)| *t == ticket) {
                s.owner = None;
            }
            s.line.retain(|(t, _)| *t != ticket);
            if s.owner.is_none() && s.line.is_empty() {
                slots.remove(key);
            }
        }
        drop(slots);
        self.changed.notify_waiters();
    }

    /// Drops kept places whose agent never came back.
    fn purge_parked(&self) {
        let stale: Vec<_> = {
            let mut parked = self.parked.lock();
            let old: Vec<_> = parked.iter().filter(|(_, (_, at))| at.elapsed() >= PARKED_FOR).map(|(k, _)| k.clone()).collect();
            old.iter().filter_map(|k| parked.remove(k)).collect()
        };
        // dropping a place hands the screen on, outside the lock
        drop(stale);
    }

    pub fn state(&self, key: &str) -> State {
        let slots = self.slots.lock();
        slots
            .get(key)
            .map(|s| State { owner: s.owner.as_ref().map(|(_, l)| l.clone()), waiting: s.line.iter().map(|(_, l)| l.clone()).collect() })
            .unwrap_or_default()
    }

    /// Takes the turn on `key` for `who`, waiting in line if someone has it.
    ///
    /// `max_wait`: give up after this long (None waits until `cancelled`). `on_wait(owner, place)`
    /// is called whenever the wait changes, for a status line. `cancelled` is polled about once a
    /// second (the agent was stopped or removed, the requester disconnected).
    pub async fn take(
        &'static self,
        key: &str,
        who: &str,
        max_wait: Option<Duration>,
        on_wait: impl Fn(&str, usize),
        cancelled: impl Fn() -> bool,
    ) -> Result<Turn> {
        self.take_as(None, key, who, max_wait, on_wait, cancelled).await
    }

    /// Like `take`, for an agent that is one connection (`conn`): when `max_wait` runs out its place in line is
    /// kept for `PARKED_FOR` instead of dropped, and asking again continues from that place. Without this an
    /// outside agent, whose call must end quickly, would go to the back of the line on every retry.
    pub async fn take_as(
        &'static self,
        conn: Option<u64>,
        key: &str,
        who: &str,
        max_wait: Option<Duration>,
        on_wait: impl Fn(&str, usize),
        cancelled: impl Fn() -> bool,
    ) -> Result<Turn> {
        self.purge_parked();
        let kept = conn.and_then(|c| self.parked.lock().remove(&(key.to_string(), c))).map(|(t, _)| t);
        let mut place = Some(match kept {
            Some(t) => t,
            None => {
                let ticket = self.next.fetch_add(1, Ordering::Relaxed);
                self.slots.lock().entry(key.to_string()).or_default().line.push_back((ticket, who.to_string()));
                // leaves the line if this future is dropped or gives up before getting the turn
                Turn { key: key.to_string(), ticket, turns: self }
            }
        });
        let ticket = place.as_ref().expect("just set").ticket;
        let start = Instant::now();
        let mut last: Option<(String, usize)> = None;
        loop {
            self.purge_parked();
            let notified = self.changed.notified();
            {
                let mut slots = self.slots.lock();
                let s = slots.entry(key.to_string()).or_default();
                if s.owner.is_none() && s.line.front().is_some_and(|(t, _)| *t == ticket) {
                    let me = s.line.pop_front().expect("front checked");
                    s.owner = Some(me);
                    drop(slots);
                    return Ok(place.take().expect("still in line"));
                }
                let owner = s.owner.as_ref().map(|(_, l)| l.clone()).unwrap_or_default();
                let pos = s.line.iter().position(|(t, _)| *t == ticket).unwrap_or(0) + 1;
                if last.as_ref() != Some(&(owner.clone(), pos)) {
                    on_wait(&owner, pos);
                    last = Some((owner, pos));
                }
            }
            if cancelled() {
                bail!("차례를 기다리다 취소되었습니다");
            }
            if let Some(m) = max_wait {
                if start.elapsed() >= m {
                    let st = self.state(key);
                    let owner = st.owner.unwrap_or_default();
                    let pos = st.waiting.iter().position(|w| w == who).map(|i| i + 1).unwrap_or(1);
                    if let (Some(c), Some(p)) = (conn, place.take()) {
                        self.parked.lock().insert((key.to_string(), c), (p, Instant::now()));
                        bail!(
                            "{owner} 이(가) 쓰는 중이라 대기열 {pos}번째입니다. 자리는 2분간 유지됩니다. 폴링하지 말고 같은 호출을 한 번만 \
                             다시 하세요 (서버가 기다리므로 토큰이 들지 않습니다). 그 사이 web/app mode 로 할 일이 있으면 먼저 하세요"
                        );
                    }
                    bail!(
                        "이 화면은 지금 {owner} 이(가) 쓰는 중입니다 (하나뿐인 화면이라 한 번에 한 에이전트만 씁니다). \
                         다른 mode(web, app 등)로 할 수 있으면 그렇게 하고, 꼭 이 화면이 필요하면 잠시 뒤 다시 desk_open 하세요"
                    );
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), notified).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn fresh() -> &'static Turns {
        Box::leak(Box::default())
    }

    #[tokio::test]
    async fn one_at_a_time_in_order() {
        let t = fresh();
        let a = t.take("k", "A", None, |_, _| {}, || false).await.unwrap();
        assert_eq!(t.state("k"), State { owner: Some("A".into()), waiting: vec![] });
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for who in ["B", "C"] {
            let o = order.clone();
            handles.push(tokio::spawn(async move {
                let turn = t.take("k", who, None, |_, _| {}, || false).await.unwrap();
                o.lock().push(who);
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(turn);
            }));
            // B lines up before C
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert_eq!(t.state("k").waiting, ["B", "C"]);
        drop(a);
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(*order.lock(), ["B", "C"]);
        assert_eq!(t.state("k"), State::default());
    }

    #[tokio::test]
    async fn other_keys_do_not_wait() {
        let t = fresh();
        let _a = t.take("child", "A", None, |_, _| {}, || false).await.unwrap();
        let b = tokio::time::timeout(Duration::from_millis(200), t.take("account:x", "B", None, |_, _| {}, || false)).await;
        assert!(b.is_ok());
    }

    #[tokio::test]
    async fn gives_up_with_the_owner_and_leaves_the_line() {
        let t = fresh();
        let _a = t.take("k", "Claude Code", None, |_, _| {}, || false).await.unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let e = t
            .take("k", "Codex", Some(Duration::from_millis(300)), move |o, p| s.lock().push((o.to_string(), p)), || false)
            .await
            .err()
            .unwrap();
        assert!(e.to_string().contains("Claude Code"), "{e}");
        assert_eq!(*seen.lock(), [("Claude Code".to_string(), 1)]);
        assert!(t.state("k").waiting.is_empty());
    }

    #[tokio::test]
    async fn cancelling_leaves_the_line() {
        let t = fresh();
        let _a = t.take("k", "A", None, |_, _| {}, || false).await.unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let h = tokio::spawn(async move { t.take("k", "B", None, |_, _| {}, move || s2.load(Ordering::SeqCst)).await.is_err() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(t.state("k").waiting, ["B"]);
        stop.store(true, Ordering::SeqCst);
        assert!(h.await.unwrap());
        assert!(t.state("k").waiting.is_empty());
    }
}

#[cfg(test)]
mod parked_tests {
    use super::*;

    #[tokio::test]
    async fn an_outside_agent_keeps_its_place_between_calls() {
        let t: &'static Turns = Box::leak(Box::default());
        let a = t.take("k", "A", None, |_, _| {}, || false).await.unwrap();
        // B (connection 7) runs out of time, then C joins behind it
        let e = t.take_as(Some(7), "k", "B", Some(Duration::from_millis(100)), |_, _| {}, || false).await.err().unwrap();
        assert!(e.to_string().contains("대기열 1번째"), "{e}");
        assert_eq!(t.state("k").waiting, ["B"], "place is kept");
        let c = tokio::spawn(async move { t.take("k", "C", None, |_, _| {}, || false).await.map(|_| ()).is_ok() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(t.state("k").waiting, ["B", "C"]);
        drop(a);
        // B asks again and is first, ahead of C
        let b = t.take_as(Some(7), "k", "B", Some(Duration::from_secs(2)), |_, _| {}, || false).await.unwrap();
        assert_eq!(t.state("k").owner.as_deref(), Some("B"));
        drop(b);
        assert!(c.await.unwrap());
    }
}

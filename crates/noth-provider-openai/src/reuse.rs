//! 「直前の同じ流れのリクエストの総入力のうち、今回キャッシュから読めた割合」を求めるための記録。
//! 流れは main→main、main→Worker の最初、Worker のステップどうしの 3 つ
use std::collections::VecDeque;
use std::sync::Mutex;

use noth_core::types::Purpose;

/// Worker の run が積み上がっても溜め込まないための上限
const MAX_WORKER_RUNS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reuse {
    pub flow: &'static str,
    pub prev_input_tokens: u64,
    pub ratio: f64,
}

#[derive(Default)]
pub struct ReuseTracker {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    main_input: Option<u64>,
    /// 古い順。上限を超えたら先頭の run から捨てる
    worker_input: VecDeque<(String, u64)>,
}

impl ReuseTracker {
    /// `input_tokens` はキャッシュ分を含む総入力。比べる相手が無い最初のリクエストと Compaction は None
    pub fn observe(
        &self,
        purpose: Purpose,
        run_id: Option<&str>,
        input_tokens: u64,
        cached_tokens: u64,
    ) -> Option<Reuse> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (flow, prev) = match (purpose, run_id) {
            (Purpose::Main | Purpose::Join, _) => {
                ("main->main", inner.main_input.replace(input_tokens))
            }
            (Purpose::Worker, Some(run)) => {
                let seen = inner.worker_input.iter().position(|(r, _)| r == run);
                let prev = seen
                    .and_then(|i| inner.worker_input.remove(i))
                    .map(|(_, p)| p);
                inner
                    .worker_input
                    .push_back((run.to_string(), input_tokens));
                if inner.worker_input.len() > MAX_WORKER_RUNS {
                    inner.worker_input.pop_front();
                }
                match prev {
                    Some(prev) => ("worker->worker", Some(prev)),
                    None => ("main->worker", inner.main_input),
                }
            }
            _ => return None,
        };
        let prev = prev.filter(|p| *p > 0)?;
        Some(Reuse {
            flow,
            prev_input_tokens: prev,
            ratio: cached_tokens as f64 / prev as f64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oldest_worker_run_is_dropped_first() {
        let t = ReuseTracker::default();
        t.observe(Purpose::Main, None, 100, 0);
        t.observe(Purpose::Worker, Some("first"), 100, 0);
        for i in 0..MAX_WORKER_RUNS {
            t.observe(Purpose::Worker, Some(&format!("r{i}")), 100, 0);
        }
        // 最も古い run だけが忘れられ、残りは前のステップを覚えている
        let forgotten = t.observe(Purpose::Worker, Some("first"), 100, 50).unwrap();
        assert_eq!(forgotten.flow, "main->worker");
        let kept = t
            .observe(
                Purpose::Worker,
                Some(&format!("r{}", MAX_WORKER_RUNS - 1)),
                100,
                50,
            )
            .unwrap();
        assert_eq!(kept.flow, "worker->worker");
    }

    #[test]
    fn follows_each_flow() {
        let t = ReuseTracker::default();
        assert!(t.observe(Purpose::Main, None, 1000, 0).is_none());
        let r = t.observe(Purpose::Main, None, 1200, 900).unwrap();
        assert_eq!((r.flow, r.prev_input_tokens), ("main->main", 1000));
        assert!((r.ratio - 0.9).abs() < 1e-9);
        let r = t.observe(Purpose::Worker, Some("r1"), 1300, 1100).unwrap();
        assert_eq!((r.flow, r.prev_input_tokens), ("main->worker", 1200));
        let r = t.observe(Purpose::Worker, Some("r1"), 1500, 1300).unwrap();
        assert_eq!((r.flow, r.prev_input_tokens), ("worker->worker", 1300));
        assert!(t.observe(Purpose::Compaction, None, 500, 0).is_none());
        let r = t.observe(Purpose::Main, None, 1600, 1200).unwrap();
        assert_eq!(r.prev_input_tokens, 1200);
    }
}

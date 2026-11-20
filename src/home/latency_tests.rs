use super::*;
fn node(n: usize) -> Target {
    Target {
        ip: format!("home-{n}"),
        port: 8327,
    }
}
#[test]
fn smoothing_freshness_sequence_and_capacity() {
    let mut history = History::default();
    let now = Instant::now();
    history.observe(node(0), Duration::from_millis(100), true, now);
    history.observe(
        node(0),
        Duration::from_millis(20),
        true,
        now + Duration::from_secs(1),
    );
    let samples = history.report(now + Duration::from_secs(2));
    assert_eq!(
        (samples[0].rtt_us, samples[0].sequence, samples[0].age_ms),
        (80000, 2, 1000)
    );
    assert!(history.report(now + Duration::from_secs(122)).is_empty());
    history.observe(
        node(0),
        Duration::from_millis(10),
        true,
        now + Duration::from_secs(180),
    );
    assert_eq!(
        history.report(now + Duration::from_secs(180))[0].rtt_us,
        10000
    );
    for n in 1..100 {
        history.observe(
            node(n),
            Duration::from_millis(1),
            true,
            now + Duration::from_secs(180 + n as u64),
        );
    }
    assert_eq!(history.entries.len(), 64);
}
#[test]
fn probe_cycles_round_against_fixed_baseline_and_failure_backoff() {
    let mut history = History::default();
    let start = Instant::now();
    let target = node(0);
    for cycle in 0..8 {
        let at = start + Duration::from_secs(cycle * 60) - Duration::from_nanos(cycle);
        assert!(history.due(&target, at));
        history.probe(
            target.clone(),
            Duration::from_millis(20),
            true,
            at,
            at + Duration::from_secs(2),
        );
        assert!(!history.due(&target, at + Duration::from_secs(2)));
    }
    let mut at = start + Duration::from_secs(600);
    for seconds in [60, 120, 240, 480, 600, 600] {
        history.probe(target.clone(), Duration::ZERO, false, at, at + TIMEOUT);
        let next = at + TIMEOUT + Duration::from_secs(seconds);
        assert!(!history.due(&target, next - Duration::from_nanos(1)));
        assert!(history.due(&target, next));
        at = next;
    }
    history.probe(target.clone(), Duration::from_millis(1), true, at, at);
    assert!(history.due(&target, at + Duration::from_secs(60)));
}
#[test]
fn cadence_jitter_matches_go_bounds() {
    for _ in 0..100 {
        assert!((Duration::from_secs(27)..Duration::from_secs(33)).contains(&sample_delay()));
        let delay = report_delay();
        assert!(delay > Duration::from_secs(60) && delay <= Duration::from_secs(120));
    }
}

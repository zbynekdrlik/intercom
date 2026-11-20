//! intercom#39 review round 1: while the hub is down the mix window retried every 3 s and logged a
//! warn line each time, also while hidden (closing only hides it): ~29 000 lines a day. The retry
//! gate warns once per outage, reloads only a visible window, and reloads a parked one on Open.

use intercom_tray::mix_window::RetryGate;

#[test]
fn only_the_first_failure_of_an_outage_warns() {
    let mut g = RetryGate::default();
    assert!(g.failed(), "the first failure warns");
    assert!(!g.failed(), "the retries do not");
    assert!(!g.failed());
    g.loaded();
    assert!(g.failed(), "a new outage after a good load warns again");
}

#[test]
fn a_hidden_window_parks_the_retry_and_open_resumes_it() {
    let mut g = RetryGate::default();
    g.failed();
    assert!(g.retry_due(true), "a visible window reloads");
    assert!(!g.retry_due(false), "a hidden window does not reload");
    assert!(g.raised(), "Open reloads the parked page at once");
    assert!(!g.raised(), "only once");
}

#[test]
fn open_without_a_parked_retry_does_not_reload() {
    let mut g = RetryGate::default();
    assert!(!g.raised());
    g.failed();
    assert!(g.retry_due(true));
    assert!(!g.raised(), "nothing parked while the window was shown");
}

#[test]
fn a_good_load_clears_a_parked_retry() {
    let mut g = RetryGate::default();
    g.failed();
    assert!(!g.retry_due(false));
    g.loaded();
    assert!(!g.raised(), "the page is up: no reload on Open");
}

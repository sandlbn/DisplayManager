//! Rule evaluation.
//!
//! Separated from execution and from the poll loop so the firing logic is
//! testable without hardware, a clock, or a running daemon: [`Automation::tick`]
//! is a pure function of the state handed to it.

use chrono::{NaiveDate, Timelike};
use display_core::rules::{parse_time, PowerState, Rule, RuleSet, Trigger};
use std::collections::HashMap;

/// A snapshot of the world, gathered by the caller.
#[derive(Debug, Clone)]
pub struct WorldState {
    /// Display selector-ish names currently attached (product names and ids).
    pub displays: Vec<String>,
    pub power: PowerState,
    /// Local wall-clock time and date.
    pub hour: u32,
    pub minute: u32,
    pub date: NaiveDate,
}

impl WorldState {
    pub fn now(displays: Vec<String>, power: PowerState) -> Self {
        let now = chrono::Local::now();
        WorldState {
            displays,
            power,
            hour: now.hour(),
            minute: now.minute(),
            date: now.date_naive(),
        }
    }
}

/// Tracks what has already been seen, so edge-triggered rules fire once.
pub struct Automation {
    rules: RuleSet,
    /// None until the first tick: the initial observation establishes a
    /// baseline. Without this, every rule would fire on daemon start as though
    /// every display had just been plugged in.
    last: Option<WorldState>,
    /// Last date each time-rule fired, so `22:00` fires once per day rather
    /// than on every poll for the rest of the evening.
    time_fired: HashMap<String, NaiveDate>,
}

impl Automation {
    pub fn new(rules: RuleSet) -> Self {
        Automation {
            rules,
            last: None,
            time_fired: HashMap::new(),
        }
    }

    pub fn rules(&self) -> &RuleSet {
        &self.rules
    }

    pub fn replace(&mut self, rules: RuleSet) {
        self.rules = rules;
        // Keep `last` so a reload does not re-fire connect rules for displays
        // that were already attached before the edit.
        self.time_fired.clear();
    }

    /// Which rules fire for this observation. Advances internal state.
    pub fn tick(&mut self, now: &WorldState) -> Vec<Rule> {
        let previous = self.last.take();
        let mut fired = Vec::new();

        for rule in self.rules.rules.iter().filter(|r| r.enabled) {
            let matched = match &rule.trigger {
                Trigger::DisplayConnected(sel) => {
                    // Edge, not level: fire on the transition only.
                    previous.as_ref().is_some_and(|p| {
                        !matches_any(&p.displays, sel) && matches_any(&now.displays, sel)
                    })
                }
                Trigger::DisplayDisconnected(sel) => previous.as_ref().is_some_and(|p| {
                    matches_any(&p.displays, sel) && !matches_any(&now.displays, sel)
                }),
                Trigger::Power(want) => previous
                    .as_ref()
                    .is_some_and(|p| p.power != now.power && now.power == *want),
                Trigger::Time(t) => self.time_due(&rule.name, t, now, previous.is_some()),
            };
            if matched {
                if let Trigger::Time(_) = rule.trigger {
                    self.time_fired.insert(rule.name.clone(), now.date);
                }
                fired.push(rule.clone());
            }
        }

        self.last = Some(now.clone());
        fired
    }

    /// True when a time rule is due and has not already fired today.
    ///
    /// Fires when the clock is at or past the target, rather than exactly on it,
    /// so a poll that lands a minute late — or a machine that was asleep at the
    /// target time — still runs the rule.
    /// `has_previous` is passed in rather than read from `self.last`: `tick`
    /// takes that field before evaluating, so reading it here would always see
    /// None and no time rule would ever fire.
    fn time_due(&self, name: &str, target: &str, now: &WorldState, has_previous: bool) -> bool {
        let Some((h, m)) = parse_time(target) else {
            return false;
        };
        if self.time_fired.get(name) == Some(&now.date) {
            return false;
        }
        // Needs a previous observation: on the first tick after a daemon start
        // at 23:00, a 22:00 rule must not fire retroactively.
        if !has_previous {
            return false;
        }
        let now_mins = now.hour * 60 + now.minute;
        let target_mins = h * 60 + m;
        now_mins >= target_mins
    }
}

/// Case-insensitive substring match against any attached display's name/id.
fn matches_any(displays: &[String], selector: &str) -> bool {
    let needle = selector.to_lowercase();
    if needle == "all" {
        return !displays.is_empty();
    }
    displays.iter().any(|d| d.to_lowercase().contains(&needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use display_core::rules::Action;

    fn rule(name: &str, trigger: Trigger) -> Rule {
        Rule {
            name: name.into(),
            trigger,
            action: Action::Profile("p".into()),
            force: false,
            enabled: true,
        }
    }

    fn state(displays: &[&str], power: PowerState, h: u32, m: u32) -> WorldState {
        WorldState {
            displays: displays.iter().map(|s| s.to_string()).collect(),
            power,
            hour: h,
            minute: m,
            date: NaiveDate::from_ymd_opt(2026, 7, 16).unwrap(),
        }
    }

    fn automation(rules: Vec<Rule>) -> Automation {
        Automation::new(RuleSet { rules })
    }

    /// The daemon starting must not look like everything was just plugged in.
    #[test]
    fn first_tick_establishes_a_baseline_and_fires_nothing() {
        let mut a = automation(vec![
            rule("c", Trigger::DisplayConnected("U2720Q".into())),
            rule("p", Trigger::Power(PowerState::Battery)),
        ]);
        let fired = a.tick(&state(&["U2720Q"], PowerState::Battery, 12, 0));
        assert!(
            fired.is_empty(),
            "nothing may fire on the first observation"
        );
    }

    #[test]
    fn display_connect_fires_once_on_the_edge() {
        let mut a = automation(vec![rule("c", Trigger::DisplayConnected("U2720Q".into()))]);
        a.tick(&state(&["MB169CK"], PowerState::Ac, 12, 0));

        let fired = a.tick(&state(&["MB169CK", "U2720Q"], PowerState::Ac, 12, 0));
        assert_eq!(fired.len(), 1);

        // Still attached: must not fire again.
        let again = a.tick(&state(&["MB169CK", "U2720Q"], PowerState::Ac, 12, 1));
        assert!(again.is_empty(), "level, not edge — fired twice");
    }

    #[test]
    fn display_disconnect_fires_on_removal() {
        let mut a = automation(vec![rule(
            "d",
            Trigger::DisplayDisconnected("U2720Q".into()),
        )]);
        a.tick(&state(&["U2720Q"], PowerState::Ac, 12, 0));
        assert_eq!(a.tick(&state(&[], PowerState::Ac, 12, 0)).len(), 1);
        assert!(a.tick(&state(&[], PowerState::Ac, 12, 0)).is_empty());
    }

    #[test]
    fn power_change_fires_only_on_transition_into_the_target() {
        let mut a = automation(vec![rule("b", Trigger::Power(PowerState::Battery))]);
        a.tick(&state(&[], PowerState::Ac, 12, 0));
        assert_eq!(a.tick(&state(&[], PowerState::Battery, 12, 0)).len(), 1);
        assert!(a.tick(&state(&[], PowerState::Battery, 12, 1)).is_empty());
        // Back to AC, then to battery again: fires again.
        a.tick(&state(&[], PowerState::Ac, 12, 2));
        assert_eq!(a.tick(&state(&[], PowerState::Battery, 12, 3)).len(), 1);
    }

    /// Unknown means "could not tell", so it must not trigger anything.
    #[test]
    fn unknown_power_never_fires_a_rule() {
        let mut a = automation(vec![
            rule("b", Trigger::Power(PowerState::Battery)),
            rule("a", Trigger::Power(PowerState::Ac)),
        ]);
        a.tick(&state(&[], PowerState::Ac, 12, 0));
        assert!(a.tick(&state(&[], PowerState::Unknown, 12, 0)).is_empty());
    }

    #[test]
    fn time_rule_fires_once_per_day() {
        let mut a = automation(vec![rule("n", Trigger::Time("22:00".into()))]);
        a.tick(&state(&[], PowerState::Ac, 21, 59));
        assert_eq!(a.tick(&state(&[], PowerState::Ac, 22, 0)).len(), 1);
        assert!(a.tick(&state(&[], PowerState::Ac, 22, 1)).is_empty());
        assert!(a.tick(&state(&[], PowerState::Ac, 23, 30)).is_empty());

        // Next day it fires again.
        let mut tomorrow = state(&[], PowerState::Ac, 22, 0);
        tomorrow.date = NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();
        assert_eq!(a.tick(&tomorrow).len(), 1);
    }

    /// A poll that lands late, or a machine asleep at the target, must still run.
    #[test]
    fn time_rule_fires_when_the_poll_is_late() {
        let mut a = automation(vec![rule("n", Trigger::Time("22:00".into()))]);
        a.tick(&state(&[], PowerState::Ac, 21, 0));
        assert_eq!(
            a.tick(&state(&[], PowerState::Ac, 22, 47)).len(),
            1,
            "a late poll must still fire the rule"
        );
    }

    /// Starting the daemon in the evening must not replay the day's rules.
    #[test]
    fn time_rule_does_not_fire_retroactively_on_startup() {
        let mut a = automation(vec![rule("n", Trigger::Time("22:00".into()))]);
        assert!(a.tick(&state(&[], PowerState::Ac, 23, 0)).is_empty());
    }

    #[test]
    fn disabled_rules_never_fire() {
        let mut r = rule("c", Trigger::DisplayConnected("U2720Q".into()));
        r.enabled = false;
        let mut a = automation(vec![r]);
        a.tick(&state(&[], PowerState::Ac, 12, 0));
        assert!(a
            .tick(&state(&["U2720Q"], PowerState::Ac, 12, 0))
            .is_empty());
    }

    #[test]
    fn selector_matching_is_case_insensitive_and_substring() {
        let mut a = automation(vec![rule("c", Trigger::DisplayConnected("u2720".into()))]);
        a.tick(&state(&[], PowerState::Ac, 12, 0));
        assert_eq!(
            a.tick(&state(&["DEL U2720Q"], PowerState::Ac, 12, 0)).len(),
            1
        );
    }

    /// Editing rules must not replay connect events for displays already there.
    #[test]
    fn reloading_rules_does_not_refire_for_already_attached_displays() {
        let mut a = automation(vec![]);
        a.tick(&state(&["U2720Q"], PowerState::Ac, 12, 0));

        a.replace(RuleSet {
            rules: vec![rule("c", Trigger::DisplayConnected("U2720Q".into()))],
        });
        assert!(
            a.tick(&state(&["U2720Q"], PowerState::Ac, 12, 1))
                .is_empty(),
            "already-attached display must not fire a connect rule after reload"
        );
    }

    #[test]
    fn multiple_rules_can_fire_from_one_observation() {
        let mut a = automation(vec![
            rule("c", Trigger::DisplayConnected("U2720Q".into())),
            rule("b", Trigger::Power(PowerState::Battery)),
        ]);
        a.tick(&state(&[], PowerState::Ac, 12, 0));
        let fired = a.tick(&state(&["U2720Q"], PowerState::Battery, 12, 0));
        assert_eq!(fired.len(), 2);
    }
}

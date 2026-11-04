//! Spreading open PRs across the roster.

use crate::distribution::Candidate;
use crate::distribution::Roster;
use claudear_core::types::AssignmentReason;

/// A PR waiting for an assignee.
#[derive(Debug, Clone)]
pub struct Pending {
    /// The PR URL.
    pub key: String,
    /// Developers with ownership of the PR, strongest first.
    pub candidates: Vec<Candidate>,
}

/// The developer a pending PR was given to.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// The PR URL.
    pub key: String,
    /// Canonical GitHub login of the assignee.
    pub assignee: String,
    /// Why the assignee was chosen.
    pub reason: AssignmentReason,
    /// The assignee's ownership score for the PR.
    pub score: f64,
}

/// Assigns pending PRs to owners without letting anyone exceed the fair share.
///
/// The fair share is the number of open PRs, both already held and pending,
/// divided by the roster size and rounded up, plus `tolerance`. Pending PRs are
/// taken strongest ownership first; each goes to its highest-ranked candidate
/// still under the fair share, and when every candidate is at or over it, to
/// the least-loaded developer. Every tie is broken by login, so the same
/// inputs always produce the same assignments.
pub struct Balancer<'a> {
    roster: &'a Roster,
    tolerance: usize,
}

impl<'a> Balancer<'a> {
    /// A balancer over `roster` allowing `tolerance` PRs above the fair share.
    pub fn new(roster: &'a Roster, tolerance: usize) -> Self {
        Self { roster, tolerance }
    }

    /// Assign every pending PR, given the logins holding the PRs already
    /// assigned (one entry per PR). Holders off the roster are ignored.
    ///
    /// Returns no decisions when the roster is empty.
    pub fn assign(&self, mut pending: Vec<Pending>, held: &[String]) -> Vec<Decision> {
        let developers = self.roster.developers();
        if developers.is_empty() {
            return Vec::new();
        }
        let mut loads = vec![0usize; developers.len()];
        for login in held {
            if let Some(index) = self.index(login) {
                loads[index] += 1;
            }
        }
        let total = loads.iter().sum::<usize>() + pending.len();
        let share = total.div_ceil(developers.len()) + self.tolerance;

        pending.sort_by(|left, right| {
            best_score(right)
                .total_cmp(&best_score(left))
                .then_with(|| left.key.cmp(&right.key))
        });

        pending
            .into_iter()
            .map(|item| {
                let owner = item.candidates.iter().find_map(|candidate| {
                    let index = self.index(&candidate.login)?;
                    (loads[index] < share).then_some((index, candidate.reason, candidate.score))
                });
                let (index, reason, score) = owner.unwrap_or_else(|| {
                    let index = self.least_loaded(&loads, &item.candidates);
                    let score = item
                        .candidates
                        .iter()
                        .find(|candidate| self.index(&candidate.login) == Some(index))
                        .map_or(0.0, |candidate| candidate.score);
                    (index, AssignmentReason::Load, score)
                });
                loads[index] += 1;
                Decision {
                    key: item.key,
                    assignee: developers[index].login.clone(),
                    reason,
                    score,
                }
            })
            .collect()
    }

    fn index(&self, login: &str) -> Option<usize> {
        let developer = self.roster.find(login)?;
        self.roster
            .developers()
            .iter()
            .position(|candidate| candidate.login == developer.login)
    }

    /// The developer with the fewest PRs, preferring a candidate with more
    /// ownership and then the earlier login.
    fn least_loaded(&self, loads: &[usize], candidates: &[Candidate]) -> usize {
        let score = |index: usize| {
            candidates
                .iter()
                .find(|candidate| self.index(&candidate.login) == Some(index))
                .map_or(0.0, |candidate| candidate.score)
        };
        (0..loads.len())
            .min_by(|&left, &right| {
                loads[left]
                    .cmp(&loads[right])
                    .then_with(|| score(right).total_cmp(&score(left)))
            })
            .unwrap_or_default()
    }
}

fn best_score(pending: &Pending) -> f64 {
    pending
        .candidates
        .first()
        .map_or(0.0, |candidate| candidate.score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distribution::Developer;
    use std::collections::HashMap;

    fn roster(logins: &[&str]) -> Roster {
        Roster::from_developers(logins.iter().map(|login| Developer::new(*login)).collect())
    }

    fn candidate(login: &str, score: f64) -> Candidate {
        Candidate {
            login: login.to_string(),
            score,
            reason: AssignmentReason::CodeOwner,
        }
    }

    fn pending(key: &str, candidates: Vec<Candidate>) -> Pending {
        Pending {
            key: key.to_string(),
            candidates,
        }
    }

    fn loads(decisions: &[Decision]) -> HashMap<&str, usize> {
        let mut loads = HashMap::new();
        for decision in decisions {
            *loads.entry(decision.assignee.as_str()).or_default() += 1;
        }
        loads
    }

    fn assignees(decisions: &[Decision]) -> Vec<(&str, &str)> {
        decisions
            .iter()
            .map(|decision| (decision.key.as_str(), decision.assignee.as_str()))
            .collect()
    }

    #[test]
    fn one_owner_of_everything_still_spreads_the_load() {
        let roster = roster(&["amy", "bob", "cat"]);
        let items = (1..=6)
            .map(|number| pending(&format!("pr-{number}"), vec![candidate("amy", 40.0)]))
            .collect();

        let decisions = Balancer::new(&roster, 0).assign(items, &[]);

        assert_eq!(decisions.len(), 6);
        assert_eq!(
            loads(&decisions),
            HashMap::from([("amy", 2), ("bob", 2), ("cat", 2)])
        );
        let owned: Vec<&Decision> = decisions
            .iter()
            .filter(|decision| decision.assignee == "amy")
            .collect();
        assert!(owned
            .iter()
            .all(|decision| decision.reason == AssignmentReason::CodeOwner));
        assert!(decisions
            .iter()
            .filter(|decision| decision.assignee != "amy")
            .all(|decision| decision.reason == AssignmentReason::Load && decision.score == 0.0));
    }

    #[test]
    fn owners_get_their_own_prs_when_under_the_share() {
        let roster = roster(&["amy", "bob"]);
        let items = vec![
            pending("pr-1", vec![candidate("bob", 40.0), candidate("amy", 5.0)]),
            pending("pr-2", vec![candidate("amy", 20.0)]),
        ];

        let decisions = Balancer::new(&roster, 0).assign(items, &[]);

        assert_eq!(
            assignees(&decisions),
            vec![("pr-1", "bob"), ("pr-2", "amy")]
        );
    }

    #[test]
    fn strongest_ownership_is_placed_first() {
        let roster = roster(&["amy", "bob"]);
        let items = vec![
            pending(
                "pr-weak",
                vec![candidate("amy", 5.0), candidate("bob", 4.0)],
            ),
            pending("pr-strong", vec![candidate("amy", 100.0)]),
        ];

        let decisions = Balancer::new(&roster, 0).assign(items, &[]);

        assert_eq!(
            assignees(&decisions),
            vec![("pr-strong", "amy"), ("pr-weak", "bob")]
        );
        assert_eq!(decisions[1].reason, AssignmentReason::CodeOwner);
    }

    #[test]
    fn held_prs_count_toward_the_share() {
        let roster = roster(&["amy", "bob"]);
        let held = vec!["AMY".to_string(), "amy".to_string(), "departed".to_string()];

        let decisions = Balancer::new(&roster, 0).assign(
            vec![
                pending("pr-1", vec![candidate("amy", 40.0)]),
                pending("pr-2", vec![candidate("amy", 40.0)]),
            ],
            &held,
        );

        assert_eq!(
            assignees(&decisions),
            vec![("pr-1", "bob"), ("pr-2", "bob")]
        );
    }

    #[test]
    fn tolerance_lets_an_owner_hold_more_than_the_share() {
        let roster = roster(&["amy", "bob"]);
        let items: Vec<Pending> = (1..=4)
            .map(|number| pending(&format!("pr-{number}"), vec![candidate("amy", 40.0)]))
            .collect();

        let strict = Balancer::new(&roster, 0).assign(items.clone(), &[]);
        let tolerant = Balancer::new(&roster, 1).assign(items, &[]);

        assert_eq!(loads(&strict), HashMap::from([("amy", 2), ("bob", 2)]));
        assert_eq!(loads(&tolerant), HashMap::from([("amy", 3), ("bob", 1)]));
    }

    #[test]
    fn unowned_prs_go_to_the_least_loaded_by_login() {
        let roster = roster(&["cat", "amy", "bob"]);

        let decisions = Balancer::new(&roster, 0).assign(
            vec![pending("pr-1", Vec::new()), pending("pr-2", Vec::new())],
            &["amy".to_string()],
        );

        assert_eq!(
            assignees(&decisions),
            vec![("pr-1", "bob"), ("pr-2", "cat")]
        );
        assert!(decisions
            .iter()
            .all(|decision| decision.reason == AssignmentReason::Load));
    }

    #[test]
    fn candidates_off_the_roster_are_skipped() {
        let roster = roster(&["amy"]);

        let decisions = Balancer::new(&roster, 0).assign(
            vec![pending(
                "pr-1",
                vec![candidate("ghost", 90.0), candidate("amy", 10.0)],
            )],
            &[],
        );

        assert_eq!(decisions[0].assignee, "amy");
        assert_eq!(decisions[0].reason, AssignmentReason::CodeOwner);
    }

    #[test]
    fn identical_inputs_give_identical_assignments() {
        let roster = roster(&["amy", "bob", "cat", "dan"]);
        let items: Vec<Pending> = (1..=9)
            .map(|number| {
                pending(
                    &format!("pr-{number}"),
                    vec![candidate("amy", 10.0), candidate("bob", 10.0)],
                )
            })
            .collect();
        let mut reversed = items.clone();
        reversed.reverse();

        let first = Balancer::new(&roster, 0).assign(items, &[]);
        let second = Balancer::new(&roster, 0).assign(reversed, &[]);

        assert_eq!(first, second);
    }

    #[test]
    fn empty_roster_assigns_nothing() {
        let roster = Roster::default();
        let decisions = Balancer::new(&roster, 0).assign(vec![pending("pr-1", Vec::new())], &[]);
        assert!(decisions.is_empty());
    }
}

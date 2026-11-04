//! Scoring how closely each developer owns a PR.

use crate::distribution::CodeOwners;
use crate::distribution::Roster;
use claudear_core::types::AssignmentReason;
use std::collections::HashMap;

/// Points for the closest requester; later requesters get a share of it by
/// position. Larger than every other signal combined, so whoever asked for
/// the work outranks anyone who merely owns the code.
const REQUESTER_WEIGHT: f64 = 100.0;
/// Points for owning every changed file in CODEOWNERS.
const CODE_OWNER_WEIGHT: f64 = 40.0;
/// Points for authoring every recent commit to the changed files.
const AUTHOR_WEIGHT: f64 = 20.0;
/// Points for authoring every recent commit to the repository.
const ACTIVITY_WEIGHT: f64 = 5.0;

/// Signals in order of strength, so ties go to the stronger one.
const SIGNALS: [AssignmentReason; 4] = [
    AssignmentReason::Requester,
    AssignmentReason::CodeOwner,
    AssignmentReason::Author,
    AssignmentReason::Activity,
];

/// What is known about who owns a PR.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    /// Logins of the people who asked for the work, closest first.
    pub requesters: Vec<String>,
    /// Paths the PR changes.
    pub files: Vec<String>,
    /// The repository's CODEOWNERS, when it has one.
    pub codeowners: Option<CodeOwners>,
    /// Author login of each recent commit to the changed files.
    pub file_authors: Vec<String>,
    /// Author login of each recent commit to the repository.
    pub repository_authors: Vec<String>,
}

/// A roster developer with some ownership of a PR.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Canonical GitHub login.
    pub login: String,
    /// Total ownership score.
    pub score: f64,
    /// The signal contributing most to the score.
    pub reason: AssignmentReason,
}

impl Evidence {
    /// Score every roster developer with any ownership, strongest first and
    /// ties broken by login so the ranking is reproducible.
    ///
    /// Each signal is weighted by the developer's share of it: the fraction
    /// of changed files CODEOWNERS gives them, or the fraction of recent
    /// commits they authored. Logins off the roster still count toward those
    /// totals but are never candidates.
    pub fn rank(&self, roster: &Roster) -> Vec<Candidate> {
        let mut points: HashMap<String, [f64; SIGNALS.len()]> = HashMap::new();
        let mut award = |login: &str, signal: AssignmentReason, value: f64| {
            if let Some(developer) = roster.find(login) {
                let index = SIGNALS
                    .iter()
                    .position(|candidate| *candidate == signal)
                    .unwrap_or_default();
                points
                    .entry(developer.login.clone())
                    .or_insert([0.0; SIGNALS.len()])[index] += value;
            }
        };

        let mut requesters: Vec<String> = Vec::new();
        for requester in &self.requesters {
            let key = requester.to_lowercase();
            if !requesters.contains(&key) {
                requesters.push(key);
            }
        }
        for (position, requester) in requesters.iter().enumerate() {
            award(
                requester,
                AssignmentReason::Requester,
                REQUESTER_WEIGHT / (position as f64 + 1.0),
            );
        }

        if let Some(codeowners) = &self.codeowners {
            let share = CODE_OWNER_WEIGHT / self.files.len().max(1) as f64;
            for file in &self.files {
                let mut owners: Vec<String> = Vec::new();
                for owner in codeowners.owners(file) {
                    let key = roster
                        .find(owner)
                        .map_or_else(|| owner.to_lowercase(), |developer| developer.login.clone());
                    if !owners.contains(&key) {
                        owners.push(key);
                    }
                }
                for owner in owners {
                    award(&owner, AssignmentReason::CodeOwner, share);
                }
            }
        }

        for (authors, signal, weight) in [
            (&self.file_authors, AssignmentReason::Author, AUTHOR_WEIGHT),
            (
                &self.repository_authors,
                AssignmentReason::Activity,
                ACTIVITY_WEIGHT,
            ),
        ] {
            let share = weight / authors.len().max(1) as f64;
            for author in authors {
                award(author, signal, share);
            }
        }

        let mut candidates: Vec<Candidate> = points
            .into_iter()
            .map(|(login, signals)| {
                let mut strongest = 0;
                for (index, value) in signals.iter().enumerate() {
                    if *value > signals[strongest] {
                        strongest = index;
                    }
                }
                Candidate {
                    login,
                    score: signals.iter().sum(),
                    reason: SIGNALS[strongest],
                }
            })
            .filter(|candidate| candidate.score > 0.0)
            .collect();
        candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.login.to_lowercase().cmp(&right.login.to_lowercase()))
        });
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distribution::Developer;

    fn roster(logins: &[&str]) -> Roster {
        Roster::from_developers(logins.iter().map(|login| Developer::new(*login)).collect())
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn ranked(candidates: &[Candidate]) -> Vec<(&str, AssignmentReason)> {
        candidates
            .iter()
            .map(|candidate| (candidate.login.as_str(), candidate.reason))
            .collect()
    }

    #[test]
    fn requester_outranks_every_other_signal_combined() {
        let evidence = Evidence {
            requesters: strings(&["amy"]),
            files: strings(&["src/lib.rs"]),
            codeowners: Some(CodeOwners::parse("* @bob")),
            file_authors: strings(&["bob", "bob"]),
            repository_authors: strings(&["bob"]),
        };

        let candidates = evidence.rank(&roster(&["amy", "bob"]));

        assert_eq!(
            ranked(&candidates),
            vec![
                ("amy", AssignmentReason::Requester),
                ("bob", AssignmentReason::CodeOwner),
            ]
        );
        assert_eq!(candidates[0].score, REQUESTER_WEIGHT);
        assert_eq!(
            candidates[1].score,
            CODE_OWNER_WEIGHT + AUTHOR_WEIGHT + ACTIVITY_WEIGHT
        );
    }

    #[test]
    fn later_requesters_get_a_smaller_share() {
        let evidence = Evidence {
            requesters: strings(&["bob", "amy", "BOB"]),
            ..Evidence::default()
        };

        let candidates = evidence.rank(&roster(&["amy", "bob"]));

        assert_eq!(candidates[0].login, "bob");
        assert_eq!(candidates[0].score, REQUESTER_WEIGHT);
        assert_eq!(candidates[1].score, REQUESTER_WEIGHT / 2.0);
    }

    #[test]
    fn codeowners_score_by_share_of_changed_files() {
        let evidence = Evidence {
            files: strings(&["docs/a.md", "src/b.rs", "src/c.rs", "src/d.rs"]),
            codeowners: Some(CodeOwners::parse("/docs/ @amy\n/src/ @bob @Bob")),
            ..Evidence::default()
        };

        let candidates = evidence.rank(&roster(&["amy", "bob"]));

        assert_eq!(candidates[0].login, "bob");
        assert_eq!(candidates[0].score, CODE_OWNER_WEIGHT * 0.75);
        assert_eq!(candidates[1].login, "amy");
        assert_eq!(candidates[1].score, CODE_OWNER_WEIGHT * 0.25);
    }

    #[test]
    fn authors_off_the_roster_dilute_but_never_appear() {
        let evidence = Evidence {
            file_authors: strings(&["claudear-bot", "claudear-bot", "claudear-bot", "amy"]),
            ..Evidence::default()
        };

        let candidates = evidence.rank(&roster(&["amy"]));

        assert_eq!(ranked(&candidates), vec![("amy", AssignmentReason::Author)]);
        assert_eq!(candidates[0].score, AUTHOR_WEIGHT / 4.0);
    }

    #[test]
    fn reason_is_the_largest_contribution() {
        let evidence = Evidence {
            files: strings(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]),
            codeowners: Some(CodeOwners::parse("/a @amy")),
            file_authors: strings(&["amy"]),
            ..Evidence::default()
        };

        let candidates = evidence.rank(&roster(&["amy"]));

        assert_eq!(candidates[0].reason, AssignmentReason::Author);
    }

    #[test]
    fn ties_break_by_login_and_unscored_developers_are_left_out() {
        let evidence = Evidence {
            repository_authors: strings(&["Cat", "bob"]),
            ..Evidence::default()
        };

        let candidates = evidence.rank(&roster(&["amy", "bob", "Cat"]));

        assert_eq!(
            ranked(&candidates),
            vec![
                ("bob", AssignmentReason::Activity),
                ("Cat", AssignmentReason::Activity),
            ]
        );
    }

    #[test]
    fn no_evidence_ranks_nobody() {
        assert!(Evidence::default().rank(&roster(&["amy"])).is_empty());
    }
}

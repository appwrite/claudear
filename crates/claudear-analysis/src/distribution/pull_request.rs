//! A snapshot of an open Claudear PR and whether it still needs someone.

use chrono::DateTime;
use chrono::Utc;

/// Lifecycle state of a PR on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// The outcome of one submitted review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

impl Verdict {
    /// Parse a GitHub review state (`APPROVED`, `CHANGES_REQUESTED`, …),
    /// treating anything unrecognised as still pending.
    pub fn from_github(state: &str) -> Self {
        match state.to_ascii_uppercase().as_str() {
            "APPROVED" => Self::Approved,
            "CHANGES_REQUESTED" => Self::ChangesRequested,
            "COMMENTED" => Self::Commented,
            "DISMISSED" => Self::Dismissed,
            _ => Self::Pending,
        }
    }

    fn is_submitted(self) -> bool {
        matches!(
            self,
            Self::Approved | Self::ChangesRequested | Self::Commented
        )
    }
}

/// One review on a PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// GitHub login of the reviewer.
    pub reviewer: String,
    /// What the review decided.
    pub verdict: Verdict,
    /// Head commit the review was submitted against.
    pub commit: Option<String>,
}

/// What, if anything, an open Claudear PR is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// Waiting on a person: it gets an assignee and is listed in the digest.
    Needed,
    /// Parked as a draft. Claudear never opens drafts, so someone chose to.
    Draft,
    /// Approved with auto-merge enabled, so GitHub merges it unattended.
    MergePending,
    /// The assignee has reviewed the current head and has not been asked to
    /// review again, so the next move is not theirs.
    Reviewed,
    /// Merged.
    Merged,
    /// Closed without merging.
    Closed,
}

impl Attention {
    /// Whether the PR is waiting on a person.
    pub fn is_needed(self) -> bool {
        self == Self::Needed
    }

    /// Whether the PR is merged or closed, ending its assignment.
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Merged | Self::Closed)
    }
}

/// A PR as the host reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// Full PR URL.
    pub url: String,
    /// `owner/repo`.
    pub repo: String,
    /// PR number.
    pub number: i64,
    /// PR title.
    pub title: String,
    /// Lifecycle state.
    pub state: PullRequestState,
    /// Whether it is a draft.
    pub draft: bool,
    /// Whether auto-merge is enabled.
    pub auto_merge: bool,
    /// Head commit SHA.
    pub head: Option<String>,
    /// Logins with an outstanding review request.
    pub requested_reviewers: Vec<String>,
    /// Submitted reviews, oldest first.
    pub reviews: Vec<Review>,
    /// When the PR was opened.
    pub created_at: Option<DateTime<Utc>>,
}

impl PullRequest {
    /// Whether the latest decisive review of every reviewer leaves the PR
    /// approved: at least one approval and no outstanding change request.
    /// Comments do not change a reviewer's decision; dismissals withdraw it.
    pub fn is_approved(&self) -> bool {
        let mut decisions: Vec<(String, Verdict)> = Vec::new();
        for review in &self.reviews {
            if !matches!(
                review.verdict,
                Verdict::Approved | Verdict::ChangesRequested | Verdict::Dismissed
            ) {
                continue;
            }
            let reviewer = review.reviewer.to_lowercase();
            match decisions.iter_mut().find(|(login, _)| *login == reviewer) {
                Some((_, verdict)) => *verdict = review.verdict,
                None => decisions.push((reviewer, review.verdict)),
            }
        }
        decisions
            .iter()
            .any(|(_, verdict)| *verdict == Verdict::Approved)
            && !decisions
                .iter()
                .any(|(_, verdict)| *verdict == Verdict::ChangesRequested)
    }

    /// What the PR is waiting on, from the point of view of `assignee` when it
    /// has one.
    pub fn attention(&self, assignee: Option<&str>) -> Attention {
        match self.state {
            PullRequestState::Merged => return Attention::Merged,
            PullRequestState::Closed => return Attention::Closed,
            PullRequestState::Open => {}
        }
        if self.draft {
            return Attention::Draft;
        }
        if self.auto_merge && self.is_approved() {
            return Attention::MergePending;
        }
        if let Some(assignee) = assignee {
            if self.has_reviewed_head(assignee) {
                return Attention::Reviewed;
            }
        }
        Attention::Needed
    }

    fn has_reviewed_head(&self, login: &str) -> bool {
        let Some(head) = self.head.as_deref() else {
            return false;
        };
        let requested = self
            .requested_reviewers
            .iter()
            .any(|reviewer| reviewer.eq_ignore_ascii_case(login));
        !requested
            && self.reviews.iter().any(|review| {
                review.reviewer.eq_ignore_ascii_case(login)
                    && review.verdict.is_submitted()
                    && review.commit.as_deref() == Some(head)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> PullRequest {
        PullRequest {
            url: "https://github.com/org/repo/pull/1".into(),
            repo: "org/repo".into(),
            number: 1,
            title: "Fix the thing".into(),
            state: PullRequestState::Open,
            draft: false,
            auto_merge: false,
            head: Some("head".into()),
            requested_reviewers: Vec::new(),
            reviews: Vec::new(),
            created_at: None,
        }
    }

    fn review(reviewer: &str, verdict: Verdict, commit: &str) -> Review {
        Review {
            reviewer: reviewer.into(),
            verdict,
            commit: Some(commit.into()),
        }
    }

    #[test]
    fn an_unreviewed_open_pr_needs_attention() {
        assert_eq!(open().attention(None), Attention::Needed);
        assert_eq!(open().attention(Some("amy")), Attention::Needed);
    }

    #[test]
    fn merged_and_closed_prs_are_finished() {
        let mut pull_request = open();
        pull_request.state = PullRequestState::Merged;
        assert_eq!(pull_request.attention(None), Attention::Merged);
        assert!(Attention::Merged.is_finished());
        pull_request.state = PullRequestState::Closed;
        assert_eq!(pull_request.attention(Some("amy")), Attention::Closed);
        assert!(Attention::Closed.is_finished());
    }

    #[test]
    fn drafts_wait() {
        let mut pull_request = open();
        pull_request.draft = true;
        assert_eq!(pull_request.attention(None), Attention::Draft);
    }

    #[test]
    fn approved_with_auto_merge_is_merge_pending() {
        let mut pull_request = open();
        pull_request.auto_merge = true;
        pull_request.reviews = vec![review("bob", Verdict::Approved, "old")];
        assert_eq!(pull_request.attention(None), Attention::MergePending);
    }

    #[test]
    fn approved_without_auto_merge_still_needs_someone_to_merge_it() {
        let mut pull_request = open();
        pull_request.reviews = vec![review("bob", Verdict::Approved, "head")];
        assert!(pull_request.is_approved());
        assert_eq!(pull_request.attention(Some("amy")), Attention::Needed);
    }

    #[test]
    fn auto_merge_without_approval_still_needs_a_review() {
        let mut pull_request = open();
        pull_request.auto_merge = true;
        assert_eq!(pull_request.attention(None), Attention::Needed);
    }

    #[test]
    fn approval_follows_each_reviewers_latest_decision() {
        let mut pull_request = open();
        pull_request.reviews = vec![
            review("bob", Verdict::ChangesRequested, "a"),
            review("bob", Verdict::Commented, "b"),
            review("BOB", Verdict::Approved, "b"),
        ];
        assert!(pull_request.is_approved());

        pull_request
            .reviews
            .push(review("cat", Verdict::ChangesRequested, "b"));
        assert!(!pull_request.is_approved());

        pull_request
            .reviews
            .push(review("cat", Verdict::Dismissed, "b"));
        assert!(pull_request.is_approved());

        pull_request.reviews = vec![review("bob", Verdict::Commented, "b")];
        assert!(!pull_request.is_approved());
    }

    #[test]
    fn assignee_who_reviewed_the_head_is_done_until_asked_again() {
        let mut pull_request = open();
        pull_request.reviews = vec![review("Amy", Verdict::ChangesRequested, "head")];
        assert_eq!(pull_request.attention(Some("amy")), Attention::Reviewed);
        assert_eq!(pull_request.attention(Some("bob")), Attention::Needed);

        pull_request.requested_reviewers = vec!["AMY".into()];
        assert_eq!(pull_request.attention(Some("amy")), Attention::Needed);
    }

    #[test]
    fn a_new_push_brings_the_assignee_back() {
        let mut pull_request = open();
        pull_request.reviews = vec![review("amy", Verdict::Approved, "older")];
        assert_eq!(pull_request.attention(Some("amy")), Attention::Needed);
    }

    #[test]
    fn pending_and_dismissed_reviews_are_not_done() {
        let mut pull_request = open();
        pull_request.reviews = vec![
            review("amy", Verdict::Pending, "head"),
            review("amy", Verdict::Dismissed, "head"),
        ];
        assert_eq!(pull_request.attention(Some("amy")), Attention::Needed);
    }

    #[test]
    fn verdicts_parse_from_github_states() {
        assert_eq!(Verdict::from_github("APPROVED"), Verdict::Approved);
        assert_eq!(
            Verdict::from_github("changes_requested"),
            Verdict::ChangesRequested
        );
        assert_eq!(Verdict::from_github("COMMENTED"), Verdict::Commented);
        assert_eq!(Verdict::from_github("DISMISSED"), Verdict::Dismissed);
        assert_eq!(Verdict::from_github("PENDING"), Verdict::Pending);
        assert_eq!(Verdict::from_github("UNKNOWN"), Verdict::Pending);
    }
}

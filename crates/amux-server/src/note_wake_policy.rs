//! Pure policy: which card events may wake a lane, and where the wake lands.
//!
//! AMUX-364a. No IO and no clock: callers pass the event kind, who wrote it,
//! and whether the card is a review card. The answer is a decision with a
//! reason string so the board log can say why a wake did or did not happen.

/// The kind of card event that could wake a lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteEventKind {
    /// The card's status changed (todo, doing, review, done, backlog...).
    StatusChanged,
    /// A review verdict was recorded on the card.
    VerdictChanged,
    /// The card's `blocked_on` field changed.
    BlockedOnChanged,
    /// A free-text note or progress entry was added.
    Note,
    /// Anything else (edits to title, labels, unrelated fields).
    Other,
}

/// Who wrote the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteAuthor {
    Human,
    Coordinator,
    Worker,
    System,
}

/// One card event, reduced to the facts the policy needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteWakeEvent {
    pub kind: NoteEventKind,
    pub author: NoteAuthor,
    /// True when the card is a review card (its lane reviews another PR).
    pub card_is_review: bool,
}

/// Outcome of the policy for one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeDecision {
    pub wake: bool,
    pub reason: &'static str,
}

impl WakeDecision {
    const fn wake(reason: &'static str) -> Self {
        Self { wake: true, reason }
    }
    const fn skip(reason: &'static str) -> Self {
        Self { wake: false, reason }
    }
}

/// Decide whether an event wakes the card's lane.
///
/// Rules, in order:
/// 1. Status, verdict and blocked_on changes always wake.
/// 2. Human and coordinator notes always wake.
/// 3. Worker or system notes (progress entries) wake only on a review card,
///    so a review lane sees its own progress; implementation cards stay quiet.
/// 4. Everything else does not wake.
pub fn evaluate(event: NoteWakeEvent) -> WakeDecision {
    match event.kind {
        NoteEventKind::StatusChanged => WakeDecision::wake("status changed"),
        NoteEventKind::VerdictChanged => WakeDecision::wake("verdict changed"),
        NoteEventKind::BlockedOnChanged => WakeDecision::wake("blocked_on changed"),
        NoteEventKind::Note => match event.author {
            NoteAuthor::Human => WakeDecision::wake("human note"),
            NoteAuthor::Coordinator => WakeDecision::wake("coordinator note"),
            NoteAuthor::Worker | NoteAuthor::System if event.card_is_review => {
                WakeDecision::wake("review progress note")
            }
            NoteAuthor::Worker | NoteAuthor::System => {
                WakeDecision::skip("progress note on a non-review card")
            }
        },
        NoteEventKind::Other => WakeDecision::skip("not a wake event"),
    }
}

/// Which card a wake for this event lands on.
///
/// A review progress note targets only the review card: `review_card_id` is
/// returned, or no target if it is absent; the implementation card is never named. Every other wake
/// targets the card the event happened on. Returns `None` when the event does
/// not wake anything.
pub fn wake_target<'a>(
    event: NoteWakeEvent,
    card_id: &'a str,
    review_card_id: Option<&'a str>,
) -> Option<&'a str> {
    if !evaluate(event).wake {
        return None;
    }
    let review_progress = event.kind == NoteEventKind::Note
        && matches!(event.author, NoteAuthor::Worker | NoteAuthor::System)
        && event.card_is_review;
    if review_progress {
        return review_card_id;
    }
    Some(card_id)
}

#[cfg(test)]
mod note_wake_policy_tests {
    use super::*;

    fn ev(kind: NoteEventKind, author: NoteAuthor, review: bool) -> NoteWakeEvent {
        NoteWakeEvent { kind, author, card_is_review: review }
    }

    #[test]
    fn status_verdict_and_blocked_on_always_wake() {
        for author in [NoteAuthor::Human, NoteAuthor::Coordinator, NoteAuthor::Worker, NoteAuthor::System] {
            for review in [false, true] {
                assert!(evaluate(ev(NoteEventKind::StatusChanged, author, review)).wake);
                assert!(evaluate(ev(NoteEventKind::VerdictChanged, author, review)).wake);
                assert!(evaluate(ev(NoteEventKind::BlockedOnChanged, author, review)).wake);
            }
        }
    }

    #[test]
    fn human_and_coordinator_notes_wake_on_any_card() {
        for review in [false, true] {
            assert!(evaluate(ev(NoteEventKind::Note, NoteAuthor::Human, review)).wake);
            assert!(evaluate(ev(NoteEventKind::Note, NoteAuthor::Coordinator, review)).wake);
        }
    }

    #[test]
    fn worker_progress_wakes_only_review_cards() {
        assert!(evaluate(ev(NoteEventKind::Note, NoteAuthor::Worker, true)).wake);
        assert!(evaluate(ev(NoteEventKind::Note, NoteAuthor::System, true)).wake);
        assert!(!evaluate(ev(NoteEventKind::Note, NoteAuthor::Worker, false)).wake);
        assert!(!evaluate(ev(NoteEventKind::Note, NoteAuthor::System, false)).wake);
    }

    #[test]
    fn other_events_never_wake() {
        for author in [NoteAuthor::Human, NoteAuthor::Coordinator, NoteAuthor::Worker, NoteAuthor::System] {
            assert!(!evaluate(ev(NoteEventKind::Other, author, true)).wake);
        }
    }

    #[test]
    fn review_progress_targets_only_the_review_card() {
        let e = ev(NoteEventKind::Note, NoteAuthor::Worker, true);
        assert_eq!(wake_target(e, "IMPL-1", Some("REVIEW-1")), Some("REVIEW-1"));
        assert_eq!(wake_target(e, "IMPL-1", None), None);
    }

    #[test]
    fn other_wakes_target_the_card_itself() {
        let e = ev(NoteEventKind::StatusChanged, NoteAuthor::System, false);
        assert_eq!(wake_target(e, "IMPL-1", Some("REVIEW-1")), Some("IMPL-1"));
        let h = ev(NoteEventKind::Note, NoteAuthor::Human, false);
        assert_eq!(wake_target(h, "IMPL-1", Some("REVIEW-1")), Some("IMPL-1"));
    }

    #[test]
    fn non_waking_events_have_no_target() {
        let e = ev(NoteEventKind::Note, NoteAuthor::Worker, false);
        assert_eq!(wake_target(e, "IMPL-1", None), None);
        assert_eq!(wake_target(ev(NoteEventKind::Other, NoteAuthor::Human, true), "R", Some("R")), None);
    }
}

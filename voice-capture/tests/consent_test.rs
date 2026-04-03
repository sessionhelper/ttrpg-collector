use serenity::all::UserId;
use ttrpg_collector::consent::manager::{ConsentScope, ConsentSession};

fn make_session(min_participants: usize, require_all: bool) -> ConsentSession {
    ConsentSession::new(
        12345,           // guild_id
        67890,           // channel_id
        11111,           // text_channel_id
        UserId::new(1),  // initiator_id
        "test-session".into(),
        min_participants,
        require_all,
    )
}

#[test]
fn test_quorum_all_accept() {
    let mut session = make_session(2, false);
    session.add_participant(UserId::new(100), "Alice".into(), false);
    session.add_participant(UserId::new(200), "Bob".into(), false);
    session.add_participant(UserId::new(300), "Carol".into(), false);

    session.record_consent(UserId::new(100), ConsentScope::Full);
    session.record_consent(UserId::new(200), ConsentScope::Full);
    session.record_consent(UserId::new(300), ConsentScope::Full);

    assert!(session.all_responded());
    assert!(session.evaluate_quorum());
    assert_eq!(session.consented_user_ids().len(), 3);
}

#[test]
fn test_quorum_one_decline_require_all() {
    let mut session = make_session(2, true); // require_all = true
    session.add_participant(UserId::new(100), "Alice".into(), false);
    session.add_participant(UserId::new(200), "Bob".into(), false);
    session.add_participant(UserId::new(300), "Carol".into(), false);

    session.record_consent(UserId::new(100), ConsentScope::Full);
    session.record_consent(UserId::new(200), ConsentScope::Decline);
    session.record_consent(UserId::new(300), ConsentScope::Full);

    assert!(session.all_responded());
    // With require_all=true, any decline means quorum fails
    assert!(!session.evaluate_quorum());
}

#[test]
fn test_quorum_min_participants() {
    let mut session = make_session(2, false); // require_all = false
    session.add_participant(UserId::new(100), "Alice".into(), false);
    session.add_participant(UserId::new(200), "Bob".into(), false);
    session.add_participant(UserId::new(300), "Carol".into(), false);
    session.add_participant(UserId::new(400), "Dave".into(), false);

    // Only 2 accept, 1 declines, 1 declines audio
    session.record_consent(UserId::new(100), ConsentScope::Full);
    session.record_consent(UserId::new(200), ConsentScope::Full);
    session.record_consent(UserId::new(300), ConsentScope::Decline);
    session.record_consent(UserId::new(400), ConsentScope::DeclineAudio);

    assert!(session.all_responded());
    // 2 full consents >= min_participants (2), require_all=false
    assert!(session.evaluate_quorum());
    assert_eq!(session.consented_user_ids().len(), 2);
}

#[test]
fn test_mid_session_join() {
    let mut session = make_session(2, false);
    session.add_participant(UserId::new(100), "Alice".into(), false);
    session.add_participant(UserId::new(200), "Bob".into(), false);

    session.record_consent(UserId::new(100), ConsentScope::Full);
    session.record_consent(UserId::new(200), ConsentScope::Full);

    assert!(session.evaluate_quorum());

    // Mid-session joiner
    session.add_participant(UserId::new(300), "Carol".into(), true);

    // Carol hasn't responded yet
    assert!(!session.all_responded());
    assert_eq!(session.pending_user_ids().len(), 1);
    assert_eq!(session.pending_user_ids()[0], UserId::new(300));

    // Verify mid_session_join flag
    let carol = session.participants.get(&UserId::new(300)).unwrap();
    assert!(carol.mid_session_join);
    assert!(carol.scope.is_none());

    // Carol consents
    session.record_consent(UserId::new(300), ConsentScope::Full);
    assert!(session.all_responded());
    assert_eq!(session.consented_user_ids().len(), 3);
}

#[test]
fn test_consent_scope_tracking() {
    let mut session = make_session(1, false);
    session.add_participant(UserId::new(100), "Alice".into(), false);
    session.add_participant(UserId::new(200), "Bob".into(), false);
    session.add_participant(UserId::new(300), "Carol".into(), false);

    // Different scopes
    session.record_consent(UserId::new(100), ConsentScope::Full);
    session.record_consent(UserId::new(200), ConsentScope::DeclineAudio);
    session.record_consent(UserId::new(300), ConsentScope::Decline);

    // consented_user_ids only returns Full scope
    let consented = session.consented_user_ids();
    assert_eq!(consented.len(), 1);
    assert_eq!(consented[0], UserId::new(100));

    // All responded
    assert!(session.all_responded());

    // has_decline should be true (Carol declined)
    assert!(session.has_decline());
}

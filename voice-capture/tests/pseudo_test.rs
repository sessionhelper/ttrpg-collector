use ttrpg_collector::storage::pseudonymize;

#[test]
fn test_pseudonymize_deterministic() {
    let user_id: u64 = 123456789012345678;
    let result1 = pseudonymize(user_id);
    let result2 = pseudonymize(user_id);
    assert_eq!(result1, result2, "Same input must always produce the same output");
}

#[test]
fn test_pseudonymize_different_inputs() {
    let id_a: u64 = 111111111111111111;
    let id_b: u64 = 222222222222222222;
    let result_a = pseudonymize(id_a);
    let result_b = pseudonymize(id_b);
    assert_ne!(
        result_a, result_b,
        "Different user IDs must produce different pseudonyms"
    );
}

#[test]
fn test_pseudonymize_length() {
    // SHA-256[0..8] = 8 bytes = 16 hex chars
    let test_ids: Vec<u64> = vec![0, 1, u64::MAX, 999999999999999999, 42];
    for id in test_ids {
        let result = pseudonymize(id);
        assert_eq!(
            result.len(),
            16,
            "Pseudonym for user_id {} should be 16 hex chars, got {} ('{}')",
            id,
            result.len(),
            result
        );
        // Also verify all characters are valid hex
        assert!(
            result.chars().all(|c| c.is_ascii_hexdigit()),
            "Pseudonym should only contain hex characters, got '{}'",
            result
        );
    }
}

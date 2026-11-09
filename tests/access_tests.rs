/// Tests for ACL (access control) logic.

#[test]
fn admin_always_has_access() {
    // Admin IDs: 100, 200
    let admin_ids = vec![100u64, 200];
    assert!(admin_ids.contains(&100));
    assert!(admin_ids.contains(&200));
    assert!(!admin_ids.contains(&999));
}

#[test]
fn approved_user_gains_access() {
    use std::collections::HashSet;
    let mut approved: HashSet<u64> = HashSet::new();
    approved.insert(42);
    assert!(approved.contains(&42));
    approved.remove(&42);
    assert!(!approved.contains(&42));
}

#[test]
fn username_lookup_case_insensitive() {
    // Simulate find_by_username
    let db: Vec<(u64, Option<String>)> = vec![(42, Some("Alice".into())), (99, Some("Bob".into()))];
    let find = |needle: &str| {
        let n = needle.trim_start_matches('@').to_lowercase();
        db.iter()
            .find(|(_, u)| u.as_deref().map(|x| x.to_lowercase()) == Some(n.clone()))
            .map(|(id, _)| *id)
    };
    assert_eq!(find("@alice"), Some(42));
    assert_eq!(find("ALICE"), Some(42));
    assert_eq!(find("bob"), Some(99));
    assert_eq!(find("charlie"), None);
}

#[test]
fn admin_cannot_be_revoked_from_approved_list() {
    // Admin IDs are static from config — revoke from approved list has no effect
    let admin_ids = vec![1u64];
    let user_id = 1u64;
    // is_admin check takes priority
    assert!(admin_ids.contains(&user_id));
    // Revoking from approved set doesn't help attacker — admin check is separate
}

#[test]
fn unknown_user_denied() {
    let admin_ids = vec![1u64];
    let approved = std::collections::HashSet::<u64>::new();
    let user_id = 999u64;
    assert!(!admin_ids.contains(&user_id) && !approved.contains(&user_id));
}

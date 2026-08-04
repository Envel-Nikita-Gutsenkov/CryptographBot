use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovedUser {
    pub id: u64,
    pub username: Option<String>,
    pub approved_by: u64,
    pub approved_at: DateTime<Utc>,
}

/// Global ACL — separate from per-user portfolio data.
/// Admin IDs come from config; additional users approved at runtime.
pub struct AccessControl {
    admin_ids: Vec<u64>,
    approved: DashMap<u64, ApprovedUser>,
}

impl AccessControl {
    pub fn new(admin_ids: Vec<u64>) -> Self {
        Self { admin_ids, approved: DashMap::new() }
    }

    /// Returns true if the user is allowed to use the bot.
    pub fn is_allowed(&self, user_id: u64) -> bool {
        self.is_admin(user_id) || self.approved.contains_key(&user_id)
    }

    pub fn is_admin(&self, user_id: u64) -> bool {
        self.admin_ids.contains(&user_id)
    }

    /// Grant access. Only admins can call this.
    pub fn approve(&self, user_id: u64, username: Option<String>, approved_by: u64) {
        self.approved.insert(user_id, ApprovedUser {
            id: user_id,
            username,
            approved_by,
            approved_at: Utc::now(),
        });
    }

    /// Revoke access. Returns true if the user existed.
    pub fn revoke(&self, user_id: u64) -> bool {
        self.approved.remove(&user_id).is_some()
    }

    /// Find approved user by @username (case-insensitive).
    pub fn find_by_username(&self, username: &str) -> Option<u64> {
        let needle = username.trim_start_matches('@').to_lowercase();
        self.approved.iter()
            .find(|e| e.username.as_deref().map(|u| u.to_lowercase()) == Some(needle.clone()))
            .map(|e| *e.key())
    }

    pub fn approved_list(&self) -> Vec<ApprovedUser> {
        let mut list: Vec<ApprovedUser> = self.approved.iter().map(|e| e.value().clone()).collect();
        list.sort_by_key(|u| u.approved_at);
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_always_allowed() {
        let acl = AccessControl::new(vec![100]);
        assert!(acl.is_allowed(100));
        assert!(!acl.is_allowed(999));
    }

    #[test]
    fn approve_revoke() {
        let acl = AccessControl::new(vec![1]);
        acl.approve(42, Some("alice".into()), 1);
        assert!(acl.is_allowed(42));
        acl.revoke(42);
        assert!(!acl.is_allowed(42));
    }

    #[test]
    fn find_by_username() {
        let acl = AccessControl::new(vec![1]);
        acl.approve(42, Some("Alice".into()), 1);
        assert_eq!(acl.find_by_username("@alice"), Some(42));
        assert_eq!(acl.find_by_username("ALICE"), Some(42));
    }

    #[test]
    fn revoke_nonexistent() {
        let acl = AccessControl::new(vec![1]);
        assert!(!acl.revoke(999));
    }
}

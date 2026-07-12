//! Data types for the admin domain.

/// One admin user (`admin_user` joined with its `authorization_role` group). Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AdminUser {
    pub id: u32,
    pub username: String,
    pub firstname: String,
    pub lastname: String,
    pub email: String,
    pub active: bool,
    /// The role (group) name; `None` = no role assigned (can't log in usefully).
    pub role: Option<String>,
    pub created: Option<String>,
    /// Last login timestamp; `None` = never logged in.
    pub last_login: Option<String>,
    /// Seconds since the last login, per the DB server's clock.
    pub last_login_secs: Option<i64>,
    pub logins: u32,
    pub failures: u32,
    /// Account is currently locked (`lock_expires` in the future).
    pub locked: bool,
    pub lock_expires: Option<String>,
    pub locale: Option<String>,
}

/// One permission rule of an admin role: an ACL resource id, allowed or denied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AdminRule {
    /// ACL resource id (`Magento_Sales::actions_view`) — resolvable via `magequery acl`.
    pub resource: String,
    pub allow: bool,
    /// Title from the static acl.xml index; `None` = no module declares it (stale rule).
    pub title: Option<String>,
}

/// One admin role (`authorization_role` group) with its members and permissions. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AdminRole {
    pub id: u32,
    pub name: String,
    /// Usernames of the admin users in this role.
    pub users: Vec<String>,
    /// The role grants everything (`Magento_Backend::all` allowed).
    pub all_resources: bool,
    pub rules: Vec<AdminRule>,
}

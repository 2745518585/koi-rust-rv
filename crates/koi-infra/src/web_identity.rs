//! Local Web account and session adapter.
//!
//! Password hashes and account records are infrastructure data.  The stable `username` is also
//! the exact `Principal.subject` handed to the core, so authorization evidence is attributable to
//! the same user users see in the Web console.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use koi_api::{
    LoginCommand, RegisterUserCommand, WebApiError, WebIdentityProvider, WebPrincipal, WebSession,
    WebUserDto,
};
use koi_core::domain::PermissionLevel;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub struct WebUserStore {
    path: PathBuf,
    users: RwLock<BTreeMap<String, StoredUser>>,
    sessions: Mutex<BTreeMap<String, StoredSession>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredUsers {
    version: u16,
    users: Vec<StoredUser>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredUser {
    email: String,
    username: String,
    password_hash: String,
    /// Web 账户只能被授予 User 或 Admin，不能通过用户库获得 System 权限。
    /// 缺少该字段的旧用户记录按 User 处理，保证向后兼容。
    #[serde(default)]
    permission: StoredUserPermission,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
enum StoredUserPermission {
    #[default]
    User,
    Admin,
}

impl StoredUserPermission {
    fn as_core_permission(self) -> PermissionLevel {
        match self {
            Self::User => PermissionLevel::User,
            Self::Admin => PermissionLevel::Admin,
        }
    }

    fn as_label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Admin => "Admin",
        }
    }
}

#[derive(Clone, Debug)]
struct StoredSession {
    username: String,
    expires_at: SystemTime,
}

const SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);

impl WebUserStore {
    /// Opens (or creates on first registration) a local account database.
    ///
    /// # Errors
    ///
    /// Returns an error when the user database cannot be read, parsed, or initialized.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, WebApiError> {
        let path = path.into();
        let users = if path.exists() {
            let body = fs::read_to_string(&path)
                .map_err(|error| WebApiError::internal(format!("读取用户库失败：{error}")))?;
            let stored: StoredUsers = serde_json::from_str(&body)
                .map_err(|error| WebApiError::internal(format!("用户库格式无效：{error}")))?;
            stored
                .users
                .into_iter()
                .map(|user| (user.username.clone(), user))
                .collect()
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path,
            users: RwLock::new(users),
            sessions: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn accepts(&self, principal: &WebPrincipal) -> bool {
        self.permission_for(&principal.subject) == Some(principal.permission)
            && self.display_name_for(&principal.subject) == principal.display_name
    }

    pub fn permission_for(&self, subject: &str) -> Option<PermissionLevel> {
        self.users
            .read()
            .ok()?
            .get(subject)
            .map(|user| user.permission.as_core_permission())
    }

    fn display_name_for(&self, subject: &str) -> Option<String> {
        self.users
            .read()
            .ok()?
            .get(subject)
            .map(|user| user.username.clone())
    }

    fn persist(&self, users: &BTreeMap<String, StoredUser>) -> Result<(), WebApiError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| WebApiError::internal(format!("创建用户库目录失败：{error}")))?;
        }
        let payload = serde_json::to_vec_pretty(&StoredUsers {
            version: 1,
            users: users.values().cloned().collect(),
        })
        .map_err(|error| WebApiError::internal(error.to_string()))?;
        fs::write(&self.path, payload)
            .map_err(|error| WebApiError::internal(format!("写入用户库失败：{error}")))
    }

    fn create_session(&self, user: &StoredUser) -> Result<WebSession, WebApiError> {
        let token = Uuid::new_v4().to_string();
        self.sessions
            .lock()
            .map_err(|_| WebApiError::Unavailable("用户会话锁不可用".into()))?
            .insert(
                token.clone(),
                StoredSession {
                    username: user.username.clone(),
                    expires_at: SystemTime::now() + SESSION_TTL,
                },
            );
        Ok(WebSession {
            token,
            principal: principal_for(user),
            user: user_dto(user),
        })
    }
}

impl WebIdentityProvider for WebUserStore {
    fn register(&self, command: RegisterUserCommand) -> Result<WebSession, WebApiError> {
        let email = normalize_email(&command.email)?;
        let username = normalize_username(&command.username)?;
        validate_password(&command.password)?;
        let mut users = self
            .users
            .write()
            .map_err(|_| WebApiError::Unavailable("用户库锁不可用".into()))?;
        if users.contains_key(&username) || users.values().any(|user| user.email == email) {
            return Err(WebApiError::conflict("邮箱或用户名已被注册"));
        }
        let salt = SaltString::generate(&mut OsRng);
        let password_hash = Argon2::default()
            .hash_password(command.password.as_bytes(), &salt)
            .map_err(|error| WebApiError::internal(format!("密码哈希失败：{error}")))?
            .to_string();
        let user = StoredUser {
            email,
            username: username.clone(),
            password_hash,
            permission: StoredUserPermission::User,
        };
        users.insert(username, user.clone());
        self.persist(&users)?;
        self.create_session(&user)
    }

    fn login(&self, command: LoginCommand) -> Result<WebSession, WebApiError> {
        let email = normalize_email(&command.email)?;
        let user = self
            .users
            .read()
            .map_err(|_| WebApiError::Unavailable("用户库锁不可用".into()))?
            .values()
            .find(|user| user.email == email)
            .cloned()
            .ok_or_else(|| WebApiError::Forbidden("邮箱或密码错误".into()))?;
        let hash = PasswordHash::new(&user.password_hash)
            .map_err(|_| WebApiError::internal("已存储的密码哈希无效"))?;
        Argon2::default()
            .verify_password(command.password.as_bytes(), &hash)
            .map_err(|_| WebApiError::Forbidden("邮箱或密码错误".into()))?;
        self.create_session(&user)
    }

    fn authenticate_session(&self, token: &str) -> Result<WebSession, WebApiError> {
        let username = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| WebApiError::Unavailable("用户会话锁不可用".into()))?;
            sessions.retain(|_, session| session.expires_at > SystemTime::now());
            sessions
                .get(token)
                .map(|session| session.username.clone())
                .ok_or_else(|| WebApiError::Forbidden("Web 会话已失效".into()))?
        };
        let user = self
            .users
            .read()
            .map_err(|_| WebApiError::Unavailable("用户库锁不可用".into()))?
            .get(&username)
            .cloned()
            .ok_or_else(|| WebApiError::Forbidden("用户已不存在".into()))?;
        Ok(WebSession {
            token: token.into(),
            principal: principal_for(&user),
            user: user_dto(&user),
        })
    }

    fn logout(&self, token: &str) -> Result<(), WebApiError> {
        self.sessions
            .lock()
            .map_err(|_| WebApiError::Unavailable("用户会话锁不可用".into()))?
            .remove(token);
        Ok(())
    }
}

fn principal_for(user: &StoredUser) -> WebPrincipal {
    WebPrincipal {
        subject: user.username.clone(),
        display_name: Some(user.username.clone()),
        permission: user.permission.as_core_permission(),
    }
}

fn user_dto(user: &StoredUser) -> WebUserDto {
    WebUserDto {
        user_id: user.username.clone(),
        username: user.username.clone(),
        email: user.email.clone(),
        permission: user.permission.as_label().into(),
    }
}

fn normalize_email(raw: &str) -> Result<String, WebApiError> {
    let email = raw.trim().to_ascii_lowercase();
    if email.len() > 254 || !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
        return Err(WebApiError::validation("请输入有效邮箱地址"));
    }
    Ok(email)
}

fn normalize_username(raw: &str) -> Result<String, WebApiError> {
    let username = raw.trim().to_ascii_lowercase();
    let valid = (3..=64).contains(&username.len())
        && username.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'_' | b'-'))
        });
    if !valid {
        return Err(WebApiError::validation(
            "用户名须为 3–64 位小写字母、数字、_ 或 -，且必须以字母或数字开头",
        ));
    }
    Ok(username)
}

fn validate_password(password: &str) -> Result<(), WebApiError> {
    if !(12..=256).contains(&password.chars().count()) {
        return Err(WebApiError::validation("密码长度须为 12–256 个字符"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_username_is_the_core_principal_subject() {
        let path = std::env::temp_dir().join(format!("koi-web-users-{}.json", Uuid::new_v4()));
        let store = WebUserStore::open(&path).unwrap();
        let session = store
            .register(RegisterUserCommand {
                email: "Ada@Example.com".into(),
                username: "ada_ops".into(),
                password: "correct horse battery staple".into(),
            })
            .unwrap();
        assert_eq!(session.principal.subject, "ada_ops");
        assert_eq!(session.user.username, session.principal.subject);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stored_admin_permission_is_exposed_to_core_and_legacy_users_stay_user() {
        let path = std::env::temp_dir().join(format!("koi-web-users-{}.json", Uuid::new_v4()));
        let body = r#"
        {
          "version": 1,
          "users": [
            {
              "email": "admin@example.com",
              "username": "admin_ops",
              "password_hash": "not-used-in-this-test",
              "permission": "Admin"
            },
            {
              "email": "legacy@example.com",
              "username": "legacy_ops",
              "password_hash": "not-used-in-this-test"
            }
          ]
        }
        "#;
        std::fs::write(&path, body).unwrap();

        let store = WebUserStore::open(&path).unwrap();
        assert_eq!(
            store.permission_for("admin_ops"),
            Some(PermissionLevel::Admin)
        );
        assert_eq!(
            store.permission_for("legacy_ops"),
            Some(PermissionLevel::User)
        );

        let admin_user = store
            .users
            .read()
            .unwrap()
            .get("admin_ops")
            .cloned()
            .unwrap();
        assert_eq!(
            principal_for(&admin_user).permission,
            PermissionLevel::Admin
        );
        assert_eq!(user_dto(&admin_user).permission, "Admin");

        std::fs::remove_file(path).unwrap();
    }
}

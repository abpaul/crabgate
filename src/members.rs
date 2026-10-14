use aes_gcm::aead::{rand_core::RngCore, OsRng};
use base64::{
    engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD},
    Engine,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Admin,
    DoorStaff,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "Admin",
            Self::DoorStaff => "Door Staff",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MemberError {
    #[error("too many sign-in links requested")]
    RateLimited,
    #[error("invalid Member role")]
    InvalidRole,
    #[error("You can't change the role of the last Admin. Add another Admin first.")]
    LastAdmin,
    #[error(transparent)]
    Database(#[from] DbErr),
}

pub struct MemberSession {
    pub member_id: String,
    pub organizer_id: String,
    pub role: Role,
}

pub struct MemberInfo {
    pub id: String,
    pub email: String,
    pub role: Role,
}

pub async fn list_members(
    control: &DatabaseConnection,
    organizer_id: &str,
) -> Result<Vec<MemberInfo>, MemberError> {
    control.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT members.id, members.email, memberships.role FROM memberships JOIN members ON members.id = memberships.member_id WHERE memberships.organizer_id = ? ORDER BY memberships.role, members.email",
        [organizer_id.into()],
    )).await?.into_iter().map(|row| {
        let role: String = row.try_get("", "role")?;
        let role = match role.as_str() {
            "Admin" => Role::Admin,
            "Door Staff" => Role::DoorStaff,
            _ => return Err(MemberError::InvalidRole),
        };
        Ok(MemberInfo { id: row.try_get("", "id")?, email: row.try_get("", "email")?, role })
    }).collect()
}

pub async fn remove_member(
    control: &DatabaseConnection,
    organizer_id: &str,
    member_id: &str,
) -> Result<bool, MemberError> {
    let affected = control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM memberships WHERE organizer_id = ? AND member_id = ?
             AND (role != 'Admin' OR (SELECT COUNT(*) FROM memberships WHERE organizer_id = ? AND role = 'Admin') > 1)",
            [organizer_id.into(), member_id.into(), organizer_id.into()],
        ))
        .await?
        .rows_affected();
    Ok(affected > 0)
}

pub async fn add_member(
    control: &DatabaseConnection,
    organizer_id: &str,
    email: &str,
    role: Role,
) -> Result<String, MemberError> {
    let email = email.trim().to_ascii_lowercase();
    let transaction = control.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO members (id, email) VALUES (?, ?) ON CONFLICT(email) DO NOTHING",
            [Uuid::new_v4().to_string().into(), email.clone().into()],
        ))
        .await?;
    let member_id: String = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM members WHERE email = ?",
            [email.clone().into()],
        ))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("Member missing after insert".to_owned()))?
        .try_get("", "id")?;
    let affected = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO memberships (member_id, organizer_id, role) VALUES (?, ?, ?)
             ON CONFLICT(member_id, organizer_id) DO UPDATE SET role = excluded.role
             WHERE memberships.role != 'Admin' OR excluded.role = 'Admin'
             OR (SELECT COUNT(*) FROM memberships WHERE organizer_id = ? AND role = 'Admin') > 1",
            [
                member_id.clone().into(),
                organizer_id.into(),
                role.as_str().into(),
                organizer_id.into(),
            ],
        ))
        .await?;
    if affected.rows_affected() == 0 {
        transaction.rollback().await?;
        return Err(MemberError::LastAdmin);
    }
    transaction.commit().await?;
    Ok(member_id)
}

pub async fn request_magic_link(
    control: &DatabaseConnection,
    organizer_id: &str,
    email: &str,
    now: i64,
) -> Result<Option<String>, MemberError> {
    let email = email.trim().to_ascii_lowercase();
    let email_hash = hash(&email);
    let affected = control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO magic_link_rate_limits (organizer_id, email_hash, hour_bucket, count)
             VALUES (?, ?, ?, 1)
             ON CONFLICT(organizer_id, email_hash, hour_bucket)
             DO UPDATE SET count = count + 1 WHERE count < 3",
            [organizer_id.into(), email_hash.into(), (now / 3600).into()],
        ))
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(MemberError::RateLimited);
    }
    let row = control
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT members.id FROM members
             JOIN memberships ON memberships.member_id = members.id
             WHERE members.email = ? AND memberships.organizer_id = ?",
            [email.into(), organizer_id.into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let member_id: String = row.try_get("", "id")?;
    let token = random_token();
    control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO magic_links (token_hash, member_id, organizer_id, expires_at, created_at)
             VALUES (?, ?, ?, ?, ?)",
            [
                hash(&token).into(),
                member_id.into(),
                organizer_id.into(),
                (now + 900).into(),
                now.into(),
            ],
        ))
        .await?;
    Ok(Some(token))
}

pub async fn claim_magic_link(
    control: &DatabaseConnection,
    organizer_id: &str,
    token: &str,
    now: i64,
) -> Result<Option<String>, MemberError> {
    let transaction = control.begin().await?;
    let token_hash = hash(token);
    let member = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT member_id FROM magic_links
             WHERE token_hash = ? AND organizer_id = ? AND used_at IS NULL AND expires_at >= ?",
            [token_hash.clone().into(), organizer_id.into(), now.into()],
        ))
        .await?;
    let Some(member) = member else {
        return Ok(None);
    };
    let member_id: String = member.try_get("", "member_id")?;
    let affected = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE magic_links SET used_at = ? WHERE token_hash = ? AND used_at IS NULL",
            [now.into(), token_hash.into()],
        ))
        .await?
        .rows_affected();
    if affected != 1 {
        return Ok(None);
    }
    let session_token = random_token();
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO member_sessions (token_hash, member_id, organizer_id, expires_at)
             VALUES (?, ?, ?, ?)",
            [
                hash(&session_token).into(),
                member_id.into(),
                organizer_id.into(),
                (now + 30 * 24 * 3600).into(),
            ],
        ))
        .await?;
    transaction.commit().await?;
    Ok(Some(session_token))
}

pub async fn create_session_for_email(
    control: &DatabaseConnection,
    organizer_id: &str,
    email: &str,
    now: i64,
) -> Result<Option<String>, MemberError> {
    let row = control
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT members.id FROM members JOIN memberships ON memberships.member_id = members.id WHERE members.email = ? AND memberships.organizer_id = ? AND memberships.role = 'Admin'",
            [email.trim().to_ascii_lowercase().into(), organizer_id.into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let member_id: String = row.try_get("", "id")?;
    let token = random_token();
    control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO member_sessions (token_hash, member_id, organizer_id, expires_at) VALUES (?, ?, ?, ?)",
            [
                hash(&token).into(),
                member_id.into(),
                organizer_id.into(),
                (now + 30 * 24 * 3600).into(),
            ],
        ))
        .await?;
    Ok(Some(token))
}

pub async fn find_session(
    control: &DatabaseConnection,
    organizer_id: &str,
    token: &str,
    now: i64,
) -> Result<Option<MemberSession>, MemberError> {
    let row = control
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT member_sessions.member_id, memberships.role
             FROM member_sessions
             JOIN memberships ON memberships.member_id = member_sessions.member_id
              AND memberships.organizer_id = member_sessions.organizer_id
             WHERE member_sessions.token_hash = ? AND member_sessions.organizer_id = ?
              AND member_sessions.expires_at > ?",
            [hash(token).into(), organizer_id.into(), now.into()],
        ))
        .await?;
    row.map(|row| {
        let role: String = row.try_get("", "role")?;
        let role = match role.as_str() {
            "Admin" => Role::Admin,
            "Door Staff" => Role::DoorStaff,
            _ => return Err(MemberError::InvalidRole),
        };
        Ok(MemberSession {
            member_id: row.try_get("", "member_id")?,
            organizer_id: organizer_id.to_owned(),
            role,
        })
    })
    .transpose()
}

pub async fn revoke_session(
    control: &DatabaseConnection,
    organizer_id: &str,
    token: &str,
) -> Result<(), MemberError> {
    control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM member_sessions WHERE organizer_id = ? AND token_hash = ?",
            [organizer_id.into(), hash(token).into()],
        ))
        .await?;
    Ok(())
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn hash(value: &str) -> String {
    STANDARD_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

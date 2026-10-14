use std::collections::{HashMap, HashSet};

use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction, DbErr, QueryResult,
    Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::{CryptoError, OrganizerCipher};

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub label: String,
    pub options: Vec<String>,
    pub required: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum QuestionError {
    #[error("Question must have a label under 160 characters")]
    InvalidLabel,
    #[error("Choose a valid question type and scope")]
    InvalidKind,
    #[error("Choice questions need 2 to 12 distinct options, each under 80 characters")]
    InvalidOptions,
    #[error("An event can have at most 20 questions")]
    Limit,
    #[error("Please answer every required question.")]
    InvalidAnswer,
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub async fn list<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
) -> Result<Vec<Question>, QuestionError> {
    list_from(db,
        "SELECT id, scope, kind, label, options_json, required FROM event_questions WHERE event_id = ? ORDER BY sort_order, id", event_id).await
}

pub async fn list_for_hold<C: ConnectionTrait>(
    db: &C,
    hold_id: &str,
) -> Result<Vec<Question>, QuestionError> {
    list_from(db,
        "SELECT question_id AS id, scope, kind, label, options_json, required FROM hold_questions WHERE hold_id = ? ORDER BY sort_order, question_id", hold_id).await
}

#[derive(Default)]
pub(crate) struct ExportQuestions {
    pub(crate) columns: Vec<Question>,
    stored_ids: HashMap<Question, HashSet<String>>,
}

impl ExportQuestions {
    pub(crate) fn answer_ids<'a>(&'a self, questions: &'a [Question]) -> Vec<&'a str> {
        questions
            .iter()
            .flat_map(|question| {
                std::iter::once(question.id.as_str()).chain(
                    self.stored_ids
                        .get(question)
                        .into_iter()
                        .flatten()
                        .map(String::as_str),
                )
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }
}

pub(crate) async fn list_for_export(
    db: &DatabaseConnection,
    event_id: &str,
) -> Result<ExportQuestions, QuestionError> {
    let mut questions = list(db, event_id).await?;
    let mut stored_ids: HashMap<Question, HashSet<String>> = HashMap::new();
    let mut seen: HashSet<Question> = questions.iter().cloned().collect();
    let historical = list_from(db,
        "WITH ranked_orders AS MATERIALIZED (SELECT hold_id, ROW_NUMBER() OVER (ORDER BY created_at, id) AS order_rank FROM orders WHERE event_id = ?), definitions AS (SELECT q.question_id AS id, q.scope, q.kind, q.label, q.options_json, q.required, MIN(o.order_rank) AS first_rank FROM hold_questions q JOIN ranked_orders o ON o.hold_id = q.hold_id GROUP BY q.question_id, q.scope, q.kind, q.label, q.options_json, q.required) SELECT d.id, d.scope, d.kind, d.label, d.options_json, d.required FROM definitions d JOIN ranked_orders o ON o.order_rank = d.first_rank JOIN hold_questions q ON q.hold_id = o.hold_id AND q.question_id = d.id ORDER BY d.first_rank, q.sort_order, d.id", event_id).await?;
    for question in historical {
        if seen.insert(question.clone()) {
            questions.push(question);
        }
    }
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "WITH ranked_orders AS MATERIALIZED (SELECT hold_id, ROW_NUMBER() OVER (ORDER BY created_at, id) AS order_rank FROM orders WHERE event_id = ?), definitions AS (SELECT a.question_snapshot_json, a.question_id, MIN(o.order_rank) AS first_rank FROM hold_answers a JOIN ranked_orders o ON o.hold_id = a.hold_id GROUP BY a.question_snapshot_json, a.question_id) SELECT d.question_snapshot_json, d.question_id FROM definitions d JOIN ranked_orders o ON o.order_rank = d.first_rank JOIN hold_answers a ON a.hold_id = o.hold_id AND a.question_id = d.question_id AND a.question_snapshot_json = d.question_snapshot_json GROUP BY d.question_snapshot_json, d.question_id, d.first_rank ORDER BY d.first_rank, d.question_id, MIN(a.ordinal)",
        [event_id.into()]
    )).await?;
    for row in rows {
        let snapshot: String = row.try_get("", "question_snapshot_json")?;
        let question: Question = serde_json::from_str(&snapshot)?;
        stored_ids
            .entry(question.clone())
            .or_default()
            .insert(row.try_get("", "question_id")?);
        if seen.insert(question.clone()) {
            questions.push(question);
        }
    }
    Ok(ExportQuestions {
        columns: questions,
        stored_ids,
    })
}

async fn list_from<C: ConnectionTrait>(
    db: &C,
    sql: &str,
    id: &str,
) -> Result<Vec<Question>, QuestionError> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            sql,
            [id.into()],
        ))
        .await?;
    rows.iter().map(question_from_row).collect()
}

fn question_from_row(row: &QueryResult) -> Result<Question, QuestionError> {
    let options: String = row.try_get("", "options_json")?;
    let required: i64 = row.try_get("", "required")?;
    Ok(Question {
        id: row.try_get("", "id")?,
        scope: row.try_get("", "scope")?,
        kind: row.try_get("", "kind")?,
        label: row.try_get("", "label")?,
        options: serde_json::from_str(&options)?,
        required: required == 1,
    })
}

pub async fn add<C: ConnectionTrait + TransactionTrait<Transaction = DatabaseTransaction>>(
    db: &C,
    event_id: &str,
    scope: &str,
    kind: &str,
    label: &str,
    options: &[String],
    required: bool,
) -> Result<String, QuestionError> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 160 {
        return Err(QuestionError::InvalidLabel);
    }
    if !matches!(scope, "Order" | "Attendee") || !matches!(kind, "Text" | "Choice" | "Checkbox") {
        return Err(QuestionError::InvalidKind);
    }
    let options: Vec<String> = options
        .iter()
        .map(|option| option.trim().to_owned())
        .collect();
    if kind == "Choice" {
        let distinct = options.iter().collect::<std::collections::HashSet<_>>();
        if !(2..=12).contains(&options.len())
            || distinct.len() != options.len()
            || options
                .iter()
                .any(|option| option.is_empty() || option.chars().count() > 80)
        {
            return Err(QuestionError::InvalidOptions);
        }
    } else if !options.is_empty() {
        return Err(QuestionError::InvalidOptions);
    }
    let transaction = db.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET updated_at = updated_at WHERE id = ?",
            [event_id.into()],
        ))
        .await?;
    let row = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM event_questions WHERE event_id = ?",
            [event_id.into()],
        ))
        .await?
        .ok_or(QuestionError::Limit)?;
    let count: i64 = row.try_get("", "count")?;
    if count >= 20 {
        return Err(QuestionError::Limit);
    }
    let id = Uuid::new_v4().to_string();
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO event_questions (id, event_id, scope, kind, label, options_json, required, sort_order) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        [id.clone().into(), event_id.into(), scope.into(), kind.into(), label.into(), serde_json::to_string(&options)?.into(), i64::from(required).into(), count.into()]
    )).await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn delete(
    db: &DatabaseConnection,
    event_id: &str,
    question_id: &str,
) -> Result<(), QuestionError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "DELETE FROM event_questions WHERE id = ? AND event_id = ?",
        [question_id.into(), event_id.into()],
    ))
    .await?;
    Ok(())
}

pub fn field_name(question: &Question, ordinal: usize) -> String {
    if question.scope == "Order" {
        format!("question_{}", question.id)
    } else {
        format!("question_{}_{}", question.id, ordinal)
    }
}

pub async fn save_answers(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    hold_id: &str,
    questions: &[Question],
    quantity: usize,
    form: &HashMap<String, String>,
) -> Result<(), QuestionError> {
    let transaction = db.begin().await?;
    save_answers_in(&transaction, cipher, hold_id, questions, quantity, form).await?;
    transaction.commit().await?;
    Ok(())
}

pub(crate) async fn save_answers_in<C: ConnectionTrait>(
    transaction: &C,
    cipher: &OrganizerCipher,
    hold_id: &str,
    questions: &[Question],
    quantity: usize,
    form: &HashMap<String, String>,
) -> Result<(), QuestionError> {
    let mut answers = Vec::new();
    for question in questions {
        let count = if question.scope == "Order" {
            1
        } else {
            quantity
        };
        for ordinal in 0..count {
            let answer = form
                .get(&field_name(question, ordinal))
                .map_or("", |value| value.trim());
            let valid = match question.kind.as_str() {
                "Text" => {
                    answer.chars().count() <= 500 && (!question.required || !answer.is_empty())
                }
                "Choice" => {
                    (answer.is_empty() && !question.required)
                        || question.options.iter().any(|option| option == answer)
                }
                "Checkbox" => answer.is_empty() && !question.required || answer == "yes",
                _ => false,
            };
            if !valid {
                return Err(QuestionError::InvalidAnswer);
            }
            if !answer.is_empty() {
                let encrypted = cipher.encrypt("question_answer", answer)?;
                answers.push((
                    question.id.clone(),
                    if question.scope == "Order" {
                        -1
                    } else {
                        ordinal as i64
                    },
                    serde_json::to_string(question)?,
                    encrypted,
                ));
            }
        }
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM hold_answers WHERE hold_id = ?",
            [hold_id.into()],
        ))
        .await?;
    for (question_id, ordinal, snapshot, answer) in answers {
        transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO hold_answers (hold_id, question_id, ordinal, question_snapshot_json, answer_encrypted) VALUES (?, ?, ?, ?, ?)",
            [hold_id.into(), question_id.into(), ordinal.into(), snapshot.into(), answer.into()]
        )).await?;
    }
    Ok(())
}

#[derive(Serialize)]
pub struct SavedAnswer {
    #[serde(skip)]
    pub(crate) question: Question,
    pub question_id: String,
    pub label: String,
    pub attendee_number: Option<i64>,
    pub answer: String,
}

pub(crate) const EXPORT_BATCH_SIZE: usize = 500;
const EXPORT_QUESTION_BATCH_SIZE: usize = EXPORT_BATCH_SIZE / 2;

pub(crate) fn export_ticket_batch_size(answer_ids: &[&str]) -> usize {
    (EXPORT_BATCH_SIZE - answer_ids.len().min(EXPORT_QUESTION_BATCH_SIZE)) / 2
}

pub(crate) enum ExportSelection<'a> {
    Orders(&'a [&'a str]),
    Tickets(&'a [(&'a str, i64)]),
}

#[derive(Default)]
pub(crate) struct ExportAnswers {
    pub(crate) asked: HashSet<Question>,
    pub(crate) answers: Vec<SavedAnswer>,
}

pub(crate) async fn export_answers(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    selection: ExportSelection<'_>,
    questions: &[Question],
    answer_ids: &[&str],
) -> Result<HashMap<String, ExportAnswers>, QuestionError> {
    let (order_ids, scope, answer_statements) = match selection {
        ExportSelection::Orders(ids) => {
            if ids.len() > EXPORT_BATCH_SIZE {
                return Err(
                    DbErr::Custom("Export question batch exceeds 500 Orders".to_owned()).into(),
                );
            }
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!("SELECT o.id AS order_id, a.rowid AS answer_rowid, a.ordinal, a.question_snapshot_json, a.answer_encrypted FROM hold_answers a JOIN orders o ON o.hold_id = a.hold_id WHERE o.id IN ({placeholders}) AND a.ordinal < 0 ORDER BY o.id, a.rowid");
            (
                ids.to_vec(),
                "Order",
                vec![Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    sql,
                    ids.iter()
                        .map(|id| (*id).into())
                        .collect::<Vec<sea_orm::Value>>(),
                )],
            )
        }
        ExportSelection::Tickets(tickets) => {
            if tickets.len() > export_ticket_batch_size(answer_ids) {
                return Err(DbErr::Custom(
                    "Export question batch exceeds the Ticket bind budget".to_owned(),
                )
                .into());
            }
            let pairs = tickets.iter().copied().collect::<HashSet<_>>();
            let order_ids = pairs
                .iter()
                .map(|(id, _)| *id)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let placeholders = vec!["(?,?)"; pairs.len()].join(",");
            let mut statements = Vec::new();
            for ids in answer_ids.chunks(EXPORT_QUESTION_BATCH_SIZE) {
                let mut values = pairs
                    .iter()
                    .flat_map(|(id, ordinal)| [(*id).into(), (*ordinal).into()])
                    .collect::<Vec<sea_orm::Value>>();
                values.extend(ids.iter().map(|id| (*id).into()));
                let ids = vec!["?"; ids.len()].join(",");
                let sql = format!("WITH selected(order_id, ordinal) AS (VALUES {placeholders}) SELECT o.id AS order_id, a.rowid AS answer_rowid, a.ordinal, a.question_snapshot_json, a.answer_encrypted FROM selected JOIN orders o ON o.id = selected.order_id JOIN hold_answers a ON a.hold_id = o.hold_id AND a.ordinal = selected.ordinal AND a.question_id IN ({ids}) ORDER BY o.id, a.rowid");
                statements.push(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    sql,
                    values,
                ));
            }
            (order_ids, "Attendee", statements)
        }
    };
    let mut result = order_ids
        .iter()
        .map(|id| ((*id).to_owned(), ExportAnswers::default()))
        .collect::<HashMap<_, _>>();
    if order_ids.is_empty() {
        return Ok(result);
    }
    let placeholders = vec!["?"; order_ids.len()].join(",");
    if questions.iter().any(|question| question.kind == "Checkbox") {
        let values = order_ids
            .iter()
            .map(|id| (*id).into())
            .collect::<Vec<sea_orm::Value>>();
        let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            format!("SELECT o.id AS order_id, q.question_id AS id, q.scope, q.kind, q.label, q.options_json, q.required FROM hold_questions q JOIN orders o ON o.hold_id = q.hold_id WHERE o.id IN ({placeholders}) AND q.scope = '{scope}' AND q.kind = 'Checkbox' ORDER BY o.id, q.sort_order, q.question_id"), values
        )).await?;
        for row in rows {
            let id: String = row.try_get("", "order_id")?;
            result
                .get_mut(&id)
                .unwrap()
                .asked
                .insert(question_from_row(&row)?);
        }
    }
    let exported = questions.iter().collect::<HashSet<_>>();
    let merge_answer_chunks = answer_statements.len() > 1;
    let mut answer_rows = Vec::new();
    for statement in answer_statements {
        for row in db.query_all_raw(statement).await? {
            let rowid = if merge_answer_chunks {
                row.try_get("", "answer_rowid")?
            } else {
                0_i64
            };
            answer_rows.push((rowid, row));
        }
    }
    if merge_answer_chunks {
        answer_rows.sort_unstable_by_key(|(rowid, _)| *rowid);
    }
    for (_, row) in answer_rows {
        let snapshot: String = row.try_get("", "question_snapshot_json")?;
        let question: Question = serde_json::from_str(&snapshot)?;
        if !exported.contains(&question) {
            continue;
        }
        let id: String = row.try_get("", "order_id")?;
        result
            .get_mut(&id)
            .unwrap()
            .answers
            .push(answer_from_snapshot(&row, cipher, question)?);
    }
    Ok(result)
}

fn answer_from_row(
    row: &QueryResult,
    cipher: &OrganizerCipher,
) -> Result<SavedAnswer, QuestionError> {
    let snapshot: String = row.try_get("", "question_snapshot_json")?;
    let question: Question = serde_json::from_str(&snapshot)?;
    answer_from_snapshot(row, cipher, question)
}

fn answer_from_snapshot(
    row: &QueryResult,
    cipher: &OrganizerCipher,
    question: Question,
) -> Result<SavedAnswer, QuestionError> {
    let ordinal: i64 = row.try_get("", "ordinal")?;
    let encrypted: String = row.try_get("", "answer_encrypted")?;
    Ok(SavedAnswer {
        question_id: question.id.clone(),
        label: question.label.clone(),
        question,
        attendee_number: (ordinal >= 0).then_some(ordinal + 1),
        answer: cipher.decrypt("question_answer", &encrypted)?,
    })
}

pub async fn order_answers(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    order_id: &str,
) -> Result<Vec<SavedAnswer>, QuestionError> {
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT a.ordinal, a.question_snapshot_json, a.answer_encrypted FROM hold_answers a JOIN orders o ON o.hold_id = a.hold_id WHERE o.id = ? ORDER BY a.rowid",
        [order_id.into()]
    )).await?;
    rows.iter()
        .map(|row| answer_from_row(row, cipher))
        .collect()
}

pub async fn asked_question_ids(
    db: &DatabaseConnection,
    order_id: &str,
) -> Result<HashSet<String>, QuestionError> {
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT q.question_id FROM hold_questions q JOIN orders o ON o.hold_id = q.hold_id WHERE o.id = ?",
        [order_id.into()]
    )).await?;
    rows.into_iter()
        .map(|row| Ok(row.try_get("", "question_id")?))
        .collect()
}

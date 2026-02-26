use chrono::{DateTime, Utc};
use ulid::Ulid;

use crate::types::{Actor, EntityType};

#[derive(Debug, Clone)]
pub struct ActivityDraft {
    pub request_id: String,
    pub actor: Actor,
    pub action: String,
    pub entity_type: Option<EntityType>,
    pub entity_id: Option<String>,
    pub file_path: Option<String>,
    pub before_revision: Option<String>,
    pub after_revision: Option<String>,
    pub summary: String,
    pub occurred_at: DateTime<Utc>,
    pub git_branch: Option<String>,
    pub git_commit: Option<String>,
}

impl ActivityDraft {
    pub fn new(actor: Actor, action: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            request_id: Ulid::new().to_string(),
            actor,
            action: action.into(),
            entity_type: None,
            entity_id: None,
            file_path: None,
            before_revision: None,
            after_revision: None,
            summary: summary.into(),
            occurred_at: Utc::now(),
            git_branch: None,
            git_commit: None,
        }
    }

    pub fn with_entity(mut self, entity_type: EntityType, entity_id: impl Into<String>) -> Self {
        self.entity_type = Some(entity_type);
        self.entity_id = Some(entity_id.into());
        self
    }

    pub fn with_path(mut self, file_path: impl Into<String>) -> Self {
        self.file_path = Some(file_path.into());
        self
    }

    pub fn with_revisions(
        mut self,
        before: Option<impl Into<String>>,
        after: Option<impl Into<String>>,
    ) -> Self {
        self.before_revision = before.map(Into::into);
        self.after_revision = after.map(Into::into);
        self
    }

    pub fn with_git(mut self, branch: Option<String>, commit: Option<String>) -> Self {
        self.git_branch = branch;
        self.git_commit = commit;
        self
    }
}

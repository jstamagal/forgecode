use std::path::Path;

use anyhow::Result;
use url::Url;

use crate::{
    AnyProvider, AuthCredential, ChatCompletionMessage, Context, Conversation, ConversationId,
    MigrationResult, Model, ModelId, Provider, ProviderId, ProviderTemplate, ResultStream, Skill,
    SnapshotCoverage, UndoOutcome, WorkspaceAuth, WorkspaceId,
};

/// Repository for the persistent undo store.
///
/// Snapshots are kept per conversation (each conversation has its own undo
/// stacks) and survive a restart of that conversation.
#[async_trait::async_trait]
pub trait SnapshotRepository: Send + Sync {
    /// Captures the current state of `path` (file, directory, symlink or
    /// absence) as the newest undo entry of the conversation.
    ///
    /// # Arguments
    /// * `conversation` - Conversation that owns the undo stack.
    /// * `path` - Absolute path about to be mutated.
    /// * `coverage` - Read coverage to restore with a file snapshot.
    ///
    /// # Returns
    /// Notices for the tool result (e.g. the entry is kept in memory only).
    ///
    /// # Errors
    /// Returns an error if the store cannot be opened.
    async fn insert_snapshot(
        &self,
        conversation: &ConversationId,
        path: &Path,
        coverage: Option<SnapshotCoverage>,
    ) -> Result<Vec<String>>;

    /// Drops the newest undo entry of `path` without restoring it, e.g. when
    /// the mutation it guarded failed.
    ///
    /// # Errors
    /// Returns an error if the store cannot be opened.
    async fn discard_snapshot(&self, conversation: &ConversationId, path: &Path) -> Result<()>;

    /// Restores the newest undo entry of `path`. The entry is removed only
    /// when the restore succeeds; an unusable entry is discarded and reported.
    ///
    /// # Errors
    /// Returns an error if the filesystem refuses the restore; the entry is
    /// kept so the undo can be retried.
    async fn undo_snapshot(
        &self,
        conversation: &ConversationId,
        path: &Path,
    ) -> Result<UndoOutcome>;

    /// Whether `path` has an undo entry in the conversation.
    ///
    /// # Errors
    /// Returns an error if the store cannot be opened.
    async fn has_snapshot(&self, conversation: &ConversationId, path: &Path) -> Result<bool>;
}

/// Repository for managing conversation persistence
///
/// This repository provides CRUD operations for conversations, including
/// creating, retrieving, and listing conversations.
#[async_trait::async_trait]
pub trait ConversationRepository: Send + Sync {
    /// Creates or updates a conversation
    ///
    /// # Arguments
    /// * `conversation` - The conversation to persist
    ///
    /// # Errors
    /// Returns an error if the operation fails
    async fn upsert_conversation(&self, conversation: Conversation) -> Result<()>;

    /// Retrieves a conversation by its ID
    ///
    /// # Arguments
    /// * `conversation_id` - The ID of the conversation to retrieve
    ///
    /// # Errors
    /// Returns an error if the operation fails
    async fn get_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<Conversation>>;

    /// Retrieves all conversations with an optional limit
    ///
    /// # Arguments
    /// * `limit` - Optional maximum number of conversations to retrieve
    ///
    /// # Errors
    /// Returns an error if the operation fails
    async fn get_all_conversations(
        &self,
        limit: Option<usize>,
    ) -> Result<Option<Vec<Conversation>>>;

    /// Retrieves the most recent conversation
    ///
    /// # Errors
    /// Returns an error if the operation fails
    async fn get_last_conversation(&self) -> Result<Option<Conversation>>;

    /// Permanently deletes a conversation
    ///
    /// # Arguments
    /// * `conversation_id` - The ID of the conversation to delete
    ///
    /// # Errors
    /// Returns an error if the operation fails
    async fn delete_conversation(&self, conversation_id: &ConversationId) -> Result<()>;
}

#[async_trait::async_trait]
pub trait ChatRepository: Send + Sync {
    async fn chat(
        &self,
        model_id: &ModelId,
        context: Context,
        provider: Provider<Url>,
    ) -> ResultStream<ChatCompletionMessage, anyhow::Error>;
    async fn models(&self, provider: Provider<Url>) -> anyhow::Result<Vec<Model>>;
}

#[async_trait::async_trait]
pub trait ProviderRepository: Send + Sync {
    async fn get_all_providers(&self) -> anyhow::Result<Vec<AnyProvider>>;
    async fn get_provider(&self, id: ProviderId) -> anyhow::Result<ProviderTemplate>;
    async fn upsert_credential(&self, credential: AuthCredential) -> anyhow::Result<()>;
    async fn get_credential(&self, id: &ProviderId) -> anyhow::Result<Option<AuthCredential>>;
    async fn remove_credential(&self, id: &ProviderId) -> anyhow::Result<()>;
    async fn migrate_env_credentials(&self) -> anyhow::Result<Option<MigrationResult>>;
}

/// Repository for managing workspace indexing and search operations
#[async_trait::async_trait]
pub trait WorkspaceIndexRepository: Send + Sync {
    /// Authenticate with the indexing service via gRPC API
    async fn authenticate(&self) -> anyhow::Result<WorkspaceAuth>;

    /// Create a new workspace on the indexing server
    async fn create_workspace(
        &self,
        working_dir: &std::path::Path,
        auth_token: &crate::ApiKey,
    ) -> anyhow::Result<WorkspaceId>;

    /// Upload files to be indexed
    async fn upload_files(
        &self,
        upload: &crate::FileUpload,
        auth_token: &crate::ApiKey,
    ) -> anyhow::Result<crate::FileUploadInfo>;

    /// Search the indexed codebase using semantic search
    async fn search(
        &self,
        query: &crate::CodeSearchQuery<'_>,
        auth_token: &crate::ApiKey,
    ) -> anyhow::Result<Vec<crate::Node>>;

    /// List all workspaces for a user
    async fn list_workspaces(
        &self,
        auth_token: &crate::ApiKey,
    ) -> anyhow::Result<Vec<crate::WorkspaceInfo>>;

    /// Get workspace information by workspace ID
    async fn get_workspace(
        &self,
        workspace_id: &WorkspaceId,
        auth_token: &crate::ApiKey,
    ) -> anyhow::Result<Option<crate::WorkspaceInfo>>;

    /// List all files in a workspace with their hashes
    async fn list_workspace_files(
        &self,
        workspace: &crate::WorkspaceFiles,
        auth_token: &crate::ApiKey,
    ) -> anyhow::Result<Vec<crate::FileHash>>;

    /// Delete files from a workspace
    async fn delete_files(
        &self,
        deletion: &crate::FileDeletion,
        auth_token: &crate::ApiKey,
    ) -> anyhow::Result<()>;

    /// Delete a workspace and all its indexed data
    async fn delete_workspace(
        &self,
        workspace_id: &WorkspaceId,
        auth_token: &crate::ApiKey,
    ) -> anyhow::Result<()>;
}

/// Repository for managing skills
///
/// This repository provides operations for loading and managing skills from
/// markdown files.
#[async_trait::async_trait]
pub trait SkillRepository: Send + Sync {
    /// Loads all available skills from the skills directory
    ///
    /// # Errors
    /// Returns an error if skill loading fails
    async fn load_skills(&self) -> Result<Vec<Skill>>;
}

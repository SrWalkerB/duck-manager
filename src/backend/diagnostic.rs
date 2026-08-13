use std::path::Path;

use async_trait::async_trait;

use super::PackageBackend;
use crate::domain::{
    BackendCapabilities, BackendError, InstalledPackage, PackageId, RemovalPlan, RemovalRequest,
    TransactionStream,
};

pub struct DiagnosticBackend {
    reason: String,
}

impl DiagnosticBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl PackageBackend for DiagnosticBackend {
    async fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::diagnostic(self.reason.clone())
    }

    async fn list_installed(&self) -> Result<Vec<InstalledPackage>, BackendError> {
        Err(BackendError::Unavailable(self.reason.clone()))
    }

    async fn get_details(&self, _ids: &[PackageId]) -> Result<Vec<InstalledPackage>, BackendError> {
        Err(BackendError::Unavailable(self.reason.clone()))
    }

    async fn find_owner(&self, _path: &Path) -> Result<Option<PackageId>, BackendError> {
        Ok(None)
    }

    async fn simulate_removal(
        &self,
        _request: RemovalRequest,
    ) -> Result<RemovalPlan, BackendError> {
        Err(BackendError::Unavailable(self.reason.clone()))
    }

    async fn remove(
        &self,
        _confirmed_plan: RemovalPlan,
    ) -> Result<TransactionStream, BackendError> {
        Err(BackendError::Unavailable(self.reason.clone()))
    }
}

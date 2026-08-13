use std::{fmt, path::PathBuf, pin::Pin};

use async_channel::Receiver;
use futures_util::Stream;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageId(String);

impl PackageId {
    pub fn new(value: impl Into<String>) -> Result<Self, BackendError> {
        let value = value.into();
        let parts: Vec<_> = value.split(';').collect();
        if parts.len() != 4 || parts[0].is_empty() {
            return Err(BackendError::InvalidPackageId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn fields(&self) -> (&str, &str, &str, &str) {
        let mut fields = self.0.splitn(4, ';');
        (
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default(),
        )
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledPackage {
    pub id: PackageId,
    pub name: String,
    pub version: String,
    pub arch: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub installed_size: Option<u64>,
    pub origin: Option<String>,
}

impl InstalledPackage {
    pub fn from_packagekit(id: PackageId, summary: Option<String>) -> Self {
        let (name, version, arch, data) = id.fields();
        let name = name.to_owned();
        let version = version.to_owned();
        let arch = arch.to_owned();
        let origin = data
            .strip_prefix("installed:")
            .filter(|origin| !origin.is_empty())
            .map(str::to_owned);
        Self {
            id,
            name,
            version,
            arch,
            summary: summary.filter(|value| !value.is_empty()),
            description: None,
            installed_size: None,
            origin,
        }
    }

    pub fn searchable_text(&self) -> String {
        [
            Some(self.name.as_str()),
            Some(self.version.as_str()),
            self.summary.as_deref(),
            self.description.as_deref(),
            self.origin.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledApplication {
    pub desktop_id: String,
    pub display_name: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub icon_name: String,
    pub desktop_file: Option<PathBuf>,
    pub executable: Option<String>,
    pub startup_wm_class: Option<String>,
    pub owner: Option<PackageId>,
    pub related_desktop_ids: Vec<String>,
}

impl InstalledApplication {
    pub fn searchable_text(&self) -> String {
        [
            Some(self.display_name.as_str()),
            self.summary.as_deref(),
            self.description.as_deref(),
            self.owner.as_ref().map(|owner| owner.fields().0),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RestartRequirement {
    #[default]
    None,
    Application,
    Session,
    System,
}

impl RestartRequirement {
    pub fn merge(self, other: Self) -> Self {
        use RestartRequirement::*;
        match (self, other) {
            (System, _) | (_, System) => System,
            (Session, _) | (_, Session) => Session,
            (Application, _) | (_, Application) => Application,
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemovalRequest {
    pub package_ids: Vec<PackageId>,
    pub allow_dependencies: bool,
    pub autoremove: bool,
}

impl RemovalRequest {
    pub fn safe(package_ids: Vec<PackageId>) -> Result<Self, BackendError> {
        if package_ids.is_empty() {
            return Err(BackendError::InvalidRequest(
                "at least one package is required".into(),
            ));
        }
        Ok(Self {
            package_ids,
            allow_dependencies: false,
            autoremove: false,
        })
    }

    pub fn validate(&self) -> Result<(), BackendError> {
        if self.allow_dependencies || self.autoremove {
            return Err(BackendError::UnsafeRequest);
        }
        if self.package_ids.is_empty() {
            return Err(BackendError::InvalidRequest(
                "at least one package is required".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemovalPlan {
    pub request: RemovalRequest,
    pub packages_to_remove: Vec<InstalledPackage>,
    pub affected_applications: Vec<String>,
    pub estimated_freed_bytes: Option<u64>,
    pub restart: RestartRequirement,
    pub warnings: Vec<String>,
}

impl RemovalPlan {
    pub fn fingerprint(&self) -> Vec<String> {
        let mut ids: Vec<_> = self
            .packages_to_remove
            .iter()
            .map(|package| package.id.as_str().to_owned())
            .collect();
        ids.sort();
        ids
    }

    pub fn equivalent_transaction(&self, other: &Self) -> bool {
        self.request == other.request && self.fingerprint() == other.fingerprint()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendKind {
    PackageKit,
    Diagnostic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendCapabilities {
    pub kind: BackendKind,
    pub backend_name: Option<String>,
    pub can_list: bool,
    pub can_simulate_removal: bool,
    pub can_remove: bool,
    pub diagnostic: Option<String>,
}

impl BackendCapabilities {
    pub fn diagnostic(message: impl Into<String>) -> Self {
        Self {
            kind: BackendKind::Diagnostic,
            backend_name: None,
            can_list: false,
            can_simulate_removal: false,
            can_remove: false,
            diagnostic: Some(message.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionEvent {
    WaitingForAuthorization,
    Started,
    PackageProgress { id: PackageId, percentage: u8 },
    OverallProgress(u8),
    RestartRequired(RestartRequirement),
    Completed,
    Failed(BackendError),
}

pub type TransactionStream = Pin<Box<dyn Stream<Item = TransactionEvent> + Send>>;

pub fn receiver_stream(receiver: Receiver<TransactionEvent>) -> TransactionStream {
    Box::pin(receiver)
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BackendError {
    #[error("package management is unavailable: {0}")]
    Unavailable(String),
    #[error("this PackageKit backend does not support the required operations")]
    Unsupported,
    #[error("PackageKit {found} is too old; version 1.3.5 or newer is required")]
    InsecureVersion { found: String },
    #[error("invalid package identifier: {0}")]
    InvalidPackageId(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("unsafe removal options were rejected")]
    UnsafeRequest,
    #[error("the removal plan changed; review it again")]
    PlanChanged,
    #[error("authorization was denied")]
    AuthorizationDenied,
    #[error("the transaction was cancelled")]
    Cancelled,
    #[error("PackageKit error {code}: {details}")]
    PackageKit { code: u32, details: String },
    #[error("D-Bus error: {0}")]
    DBus(String),
}

pub fn format_size(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "—".into();
    };
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(id: &str) -> InstalledPackage {
        InstalledPackage::from_packagekit(PackageId::new(id).unwrap(), None)
    }

    #[test]
    fn package_id_is_opaque_but_exposes_display_fields() {
        let id =
            PackageId::new("google-chrome-stable;151.0-1;x86_64;installed:google-chrome").unwrap();
        assert_eq!(id.fields().0, "google-chrome-stable");
        assert_eq!(
            id.as_str(),
            "google-chrome-stable;151.0-1;x86_64;installed:google-chrome"
        );
    }

    #[test]
    fn package_id_rejects_an_incomplete_value() {
        assert!(PackageId::new("broken-package").is_err());
    }

    #[test]
    fn safe_request_never_allows_dependency_removal_or_autoremove() {
        let request =
            RemovalRequest::safe(vec![PackageId::new("app;1;x86_64;installed").unwrap()]).unwrap();
        assert!(!request.allow_dependencies);
        assert!(!request.autoremove);
        assert!(request.validate().is_ok());
    }

    #[test]
    fn changed_transaction_requires_a_new_confirmation() {
        let request =
            RemovalRequest::safe(vec![PackageId::new("app;1;x86_64;installed").unwrap()]).unwrap();
        let first = RemovalPlan {
            request: request.clone(),
            packages_to_remove: vec![package("app;1;x86_64;installed")],
            affected_applications: vec![],
            estimated_freed_bytes: None,
            restart: RestartRequirement::None,
            warnings: vec![],
        };
        let mut changed = first.clone();
        changed
            .packages_to_remove
            .push(package("lib;1;x86_64;installed"));
        assert!(!first.equivalent_transaction(&changed));
    }

    #[test]
    fn size_format_is_decimal_and_readable() {
        assert_eq!(format_size(Some(451_509_313)), "451.5 MB");
        assert_eq!(format_size(None), "—");
    }
}

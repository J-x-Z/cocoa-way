use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    Ready,
    Invalid,
    Blocked,
}

impl ProfileStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Invalid => "Invalid profile",
            Self::Blocked => "Blocked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    Starting,
    Running,
    Stopping,
    Exited,
    Failed,
}

impl InstanceStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
            Self::Exited => "Exited",
            Self::Failed => "Failed",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    Unavailable,
    Starting,
    Ready,
    Degraded,
    Stopping,
    Failed,
}

impl RuntimeStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unavailable => "Unavailable",
            Self::Starting => "Starting",
            Self::Ready => "Ready",
            Self::Degraded => "Degraded",
            Self::Stopping => "Stopping",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayStatus {
    Free,
    Allocating,
    Attached,
    Closing,
    Failed,
}

impl DisplayStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Free => "Free",
            Self::Allocating => "Allocating",
            Self::Attached => "Attached",
            Self::Closing => "Closing",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Running => "Running",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchStep {
    ValidateProfile,
    CheckRuntime,
    CheckImage,
    CreateContainer,
    AllocateDisplay,
    StartWaypipe,
    StartCommand,
    MarkRunning,
}

impl LaunchStep {
    pub const ALL: [Self; 8] = [
        Self::ValidateProfile,
        Self::CheckRuntime,
        Self::CheckImage,
        Self::CreateContainer,
        Self::AllocateDisplay,
        Self::StartWaypipe,
        Self::StartCommand,
        Self::MarkRunning,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ValidateProfile => "Validate profile",
            Self::CheckRuntime => "Check runtime",
            Self::CheckImage => "Check image",
            Self::CreateContainer => "Create container",
            Self::AllocateDisplay => "Allocate display",
            Self::StartWaypipe => "Start Waypipe",
            Self::StartCommand => "Start application",
            Self::MarkRunning => "Mark instance running",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskStep {
    pub name: String,
    pub status: TaskStepStatus,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationTask {
    pub id: u64,
    pub key: String,
    pub operation: String,
    pub subject: String,
    pub status: TaskStatus,
    pub steps: Vec<TaskStep>,
    pub detail: Option<String>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplicationInstanceSnapshot {
    pub id: u64,
    pub profile_index: usize,
    pub status: InstanceStatus,
    pub started_at_unix_ms: u128,
    pub container_pid: Option<u32>,
    pub waypipe_pid: u32,
    pub display_slot: String,
    pub display_pid: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_states_have_unambiguous_labels() {
        assert_eq!(ProfileStatus::Invalid.label(), "Invalid profile");
        assert_eq!(InstanceStatus::Stopping.label(), "Stopping");
        assert_eq!(RuntimeStatus::Degraded.label(), "Degraded");
        assert_eq!(DisplayStatus::Attached.label(), "Attached");
    }

    #[test]
    fn launch_pipeline_matches_the_product_order() {
        let labels = LaunchStep::ALL.map(LaunchStep::label);
        assert_eq!(labels.first(), Some(&"Validate profile"));
        assert_eq!(labels.last(), Some(&"Mark instance running"));
        assert_eq!(labels.len(), 8);
    }
}

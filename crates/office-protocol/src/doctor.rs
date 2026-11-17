use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DoctorCheck {
    pub component: String,
    pub status: DoctorStatus,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Passed,
    Failed,
    Skipped,
}

impl DoctorReport {
    pub fn passed(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.status != DoctorStatus::Failed)
    }

    pub fn add(&mut self, component: impl Into<String>, result: Result<String, String>) {
        let (status, detail) = match result {
            Ok(detail) => (DoctorStatus::Passed, detail),
            Err(detail) => (DoctorStatus::Failed, detail),
        };
        self.checks.push(DoctorCheck {
            component: component.into(),
            status,
            detail,
        });
    }

    pub fn skip(&mut self, component: impl Into<String>, reason: impl Into<String>) {
        self.checks.push(DoctorCheck {
            component: component.into(),
            status: DoctorStatus::Skipped,
            detail: reason.into(),
        });
    }
}

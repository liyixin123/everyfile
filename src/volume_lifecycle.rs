use std::collections::HashMap;
use std::path::PathBuf;

use crate::model::{Coverage, Freshness};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedVolume {
    pub identity: String,
    pub mount_path: PathBuf,
    pub freshness: Freshness,
    pub coverage: Option<Coverage>,
    pub reconciliation_in_flight: bool,
}

#[derive(Default)]
pub struct VolumeLifecycle {
    volumes: HashMap<String, ManagedVolume>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountTransition {
    Unchanged,
    Connected,
    Reconnected,
    IdentityMismatch,
}

impl VolumeLifecycle {
    pub fn restore_committed(
        &mut self,
        identity: String,
        mount_path: PathBuf,
        coverage: Coverage,
        mounted: bool,
    ) {
        self.volumes.insert(
            identity.clone(),
            ManagedVolume {
                identity,
                mount_path,
                freshness: if mounted {
                    Freshness::Current
                } else {
                    Freshness::Offline
                },
                coverage: Some(coverage),
                reconciliation_in_flight: false,
            },
        );
    }

    pub fn removed(&mut self, identity: &str) -> bool {
        let Some(volume) = self.volumes.get_mut(identity) else {
            return false;
        };
        volume.freshness = Freshness::Offline;
        volume.reconciliation_in_flight = false;
        true
    }

    pub fn mounted(&mut self, identity: &str, mount_path: PathBuf) -> MountTransition {
        if self
            .volumes
            .values()
            .any(|volume| volume.identity != identity && volume.mount_path == mount_path)
        {
            return MountTransition::IdentityMismatch;
        }
        match self.volumes.get_mut(identity) {
            Some(volume) => {
                let reconnected = volume.freshness == Freshness::Offline;
                volume.mount_path = mount_path;
                volume.freshness = if reconnected {
                    Freshness::CatchingUp
                } else {
                    volume.freshness
                };
                if reconnected {
                    MountTransition::Reconnected
                } else {
                    MountTransition::Unchanged
                }
            }
            None => MountTransition::Connected,
        }
    }

    pub fn begin_reconciliation(&mut self, identity: &str) -> bool {
        let Some(volume) = self.volumes.get_mut(identity) else {
            return false;
        };
        if volume.freshness == Freshness::Offline {
            return false;
        }
        volume.reconciliation_in_flight = true;
        volume.freshness = Freshness::CatchingUp;
        true
    }

    pub fn committed(&mut self, identity: &str, coverage: Coverage) -> bool {
        let Some(volume) = self.volumes.get_mut(identity) else {
            return false;
        };
        volume.coverage = Some(coverage);
        volume.freshness = Freshness::Current;
        volume.reconciliation_in_flight = false;
        true
    }

    pub fn get(&self, identity: &str) -> Option<&ManagedVolume> {
        self.volumes.get(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_during_reconciliation_keeps_coverage_and_becomes_offline() {
        let mut lifecycle = VolumeLifecycle::default();
        lifecycle.restore_committed(
            "disk".into(),
            "/Volumes/Disk".into(),
            Coverage::Partial,
            true,
        );
        assert!(lifecycle.begin_reconciliation("disk"));
        assert!(lifecycle.removed("disk"));
        let volume = lifecycle.get("disk").unwrap();
        assert_eq!(volume.freshness, Freshness::Offline);
        assert_eq!(volume.coverage, Some(Coverage::Partial));
        assert!(!volume.reconciliation_in_flight);
    }

    #[test]
    fn matching_identity_reconnects_but_reused_path_does_not() {
        let mut lifecycle = VolumeLifecycle::default();
        lifecycle.restore_committed(
            "original".into(),
            "/Volumes/Disk".into(),
            Coverage::Complete,
            false,
        );
        assert_eq!(
            lifecycle.mounted("original", "/Volumes/Renamed".into()),
            MountTransition::Reconnected
        );
        assert_eq!(
            lifecycle.get("original").unwrap().freshness,
            Freshness::CatchingUp
        );
        assert_eq!(
            lifecycle.mounted("different", "/Volumes/Renamed".into()),
            MountTransition::IdentityMismatch
        );
    }
}

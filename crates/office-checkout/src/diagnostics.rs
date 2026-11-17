use std::{fs::File, io::Read, path::Path};

use office_protocol::{
    CheckoutCoordinatorLease, CheckoutHostHealth, DoctorReport, ScopeDeployment,
    ScopeDeploymentProof, SourceEntry,
};
use rustix::fs::{self, Mode, OFlags};
use sha2::{Digest, Sha256};

use crate::{
    CheckoutCoordinator, PolicyStateError,
    linux::snapshot_source,
    policy_state::verify_file,
    publication::{read_record, verify_journal},
    query,
};

pub fn check_local_checkout(
    root: &Path,
    state_dir: &Path,
    authority: &CheckoutHostHealth,
    require_scope: bool,
    report: &mut DoctorReport,
) {
    let local = match CheckoutCoordinator::bind(root, state_dir, &authority.root) {
        Ok(local) => {
            report.add(
                "root",
                Ok("Verified directory identity and owner-only state".into()),
            );
            local
        }
        Err(error) => {
            report.add("root", Err(error.to_string()));
            return;
        }
    };
    report.add("coordinator", verify_epoch(&local, authority));
    report.add("journal", repository_journal(&local, authority));
    let entries =
        snapshot_source(&local.root, &authority.manifest).map_err(|error| error.to_string());
    report.add(
        "source_head",
        match (&entries, &authority.head) {
            (Ok((baseline, _)), Some(expected))
                if baseline.manifest_sha256 == expected.source_sha256 =>
            {
                Ok(format!(
                    "Checkout revision {} · {}",
                    expected.checkout_revision, expected.source_sha256
                ))
            }
            (Err(error), _) => Err(error.clone()),
            _ => Err(
                "Local sources differ from the registered head or the head is unavailable".into(),
            ),
        },
    );
    match (require_scope, &authority.scope, &entries) {
        (_, Some(proof), Ok((_, entries))) => report.add(
            "scoped_read_view",
            verify_scope(&local, authority, proof, entries),
        ),
        (true, _, _) => report.add(
            "scoped_read_view",
            Err("No verifiable active scope for the selected task".into()),
        ),
        _ => report.skip(
            "scoped_read_view",
            "Pass --scope-task to verify an active deployed scope",
        ),
    }
}

fn verify_epoch(
    local: &CheckoutCoordinator,
    authority: &CheckoutHostHealth,
) -> Result<String, String> {
    let lease = authority
        .lease
        .as_ref()
        .filter(|lease| lease.ready_at.is_some())
        .ok_or("There is no ready coordinator with a live server lease")?;
    let observed: CheckoutCoordinatorLease =
        read_record(&local.state, &format!("coordinator-{}.json", lease.epoch))
            .map_err(|error| error.to_string())?
            .ok_or("The server epoch has no local receipt")?;
    if observed.epoch != lease.epoch
        || observed.checkout_id != lease.checkout_id
        || observed.runner_id != lease.runner_id
        || observed.started_at != lease.started_at
        || observed.workspace_id != lease.workspace_id
    {
        return Err("Local epoch receipt differs from server authority".into());
    }
    match local.lock() {
        Err(PolicyStateError::CheckoutBusy) => Ok(format!(
            "Active epoch {} · exclusive checkout lock held",
            lease.epoch
        )),
        Ok(()) => Err("No coordinator is holding the local checkout lock".into()),
        Err(error) => Err(error.to_string()),
    }
}

fn repository_journal(
    local: &CheckoutCoordinator,
    authority: &CheckoutHostHealth,
) -> Result<String, String> {
    if authority.needs_reconcile || authority.applies_pending {
        return Err("The server reports a pending publication or reconciliation".into());
    }
    let state_id = authority
        .state_id
        .ok_or("The active epoch has no publication journal receipt")?;
    verify_journal(local, authority.checkout_id, state_id).map_err(|error| error.to_string())?;
    Ok(format!("Journal {state_id} · no pending operation"))
}

fn verify_scope(
    local: &CheckoutCoordinator,
    authority: &CheckoutHostHealth,
    expected: &ScopeDeploymentProof,
    entries: &[SourceEntry],
) -> Result<String, String> {
    let name = format!("scope-{}-{}.json", expected.scope_id, expected.revision);
    let fd = fs::openat(
        &local.state,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| "The approved scope deployment is missing")?;
    verify_file(&fd).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    File::from(fd)
        .take(128 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the scope receipt")?;
    if hex::encode(Sha256::digest(&bytes)) != expected.deployment_sha256 {
        return Err("Local scope bytes differ from the acknowledged deployment".into());
    }
    let scope: ScopeDeployment =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid scope receipt")?;
    if scope.scope.checkout_id != authority.checkout_id
        || scope.scope.scope_id != expected.scope_id
        || scope.scope.revision != expected.revision
    {
        return Err("The scope receipt belongs to a different checkout or revision".into());
    }
    let read = &scope.scope.permissions.read;
    let mut allowed: Vec<_> = entries
        .iter()
        .map(|file| &file.path)
        .filter(|path| read.allows(path))
        .collect();
    allowed.sort_unstable();
    let observed = query::paths(&local.root, &authority.manifest, read, None)
        .map_err(|reason| format!("Could not enumerate the deployed read view: {reason:?}"))?;
    if observed.iter().ne(allowed.iter().copied()) {
        return Err("The gateway read view differs from the deployed scope".into());
    }
    Ok(format!(
        "Scope revision {} · {} visible files",
        expected.revision,
        observed.len()
    ))
}

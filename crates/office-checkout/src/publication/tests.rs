use std::{fs, os::unix::fs::symlink, path::PathBuf};

use chrono::{Duration, Utc};
use office_domain::{
    AgentId, PathAccess, PathRule, TaskId, UserId, WorkPermissions, WorkspaceId, WriteOperation,
};
use office_protocol::{
    ApplyCheckoutChanges, CheckoutContent, CheckoutDependency, FileEncoding, Recipient,
    RootIdentity, WorkScopeDefinition, WorkScopeOperation,
};
use tempfile::TempDir;

use super::*;
use crate::inspect_checkout;

struct Fixture {
    _temporary: TempDir,
    tree: PathBuf,
    state: PathBuf,
    manifest: SourceManifest,
    identity: RootIdentity,
    checkout_id: Uuid,
    head: CheckoutContentHead,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let tree = temporary.path().join("source");
        let state = temporary.path().join("private");
        fs::create_dir(&tree).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
        fs::write(tree.join("alpha.txt"), "alpha before").unwrap();
        fs::write(tree.join("beta.txt"), "beta before").unwrap();
        let manifest = SourceManifest {
            include: vec![PathRule::Tree],
            exclude: Vec::new(),
        };
        let proof = inspect_checkout(&tree, &manifest).unwrap().proof;
        let identity = RootIdentity {
            host_fingerprint: proof.host_fingerprint,
            directory_id: proof.directory_id,
            ancestors: proof.ancestors,
        };
        let head = CheckoutContentHead {
            checkout_revision: 0,
            source_sha256: proof.baseline.manifest_sha256,
        };
        Self {
            _temporary: temporary,
            tree,
            state,
            manifest,
            identity,
            checkout_id: Uuid::new_v4(),
            head,
        }
    }

    fn lock(&self) -> CheckoutCoordinator {
        CheckoutCoordinator::open(&self.tree, &self.state, &self.identity).unwrap()
    }

    fn publisher<'a>(&'a self, local: &'a CheckoutCoordinator) -> CheckoutPublisher<'a> {
        CheckoutPublisher::open(local, self.checkout_id, &self.manifest).unwrap()
    }

    fn order(
        &self,
        previous: &CheckoutContentHead,
        operations: Vec<CheckoutChange>,
    ) -> CheckoutWriteOrder {
        let allow_tree = PathAccess {
            allow: vec![PathRule::Tree],
            deny: Vec::new(),
        };
        CheckoutWriteOrder {
            change_id: Uuid::new_v4(),
            epoch: 1,
            permit_until: Utc::now() + Duration::seconds(30),
            previous: previous.clone(),
            input: ApplyCheckoutChanges {
                compensates_change_id: None,
                scope_revision: 1,
                base_checkout_revision: 0,
                claim_fence: None,
                idempotency_key: Uuid::new_v4(),
                operations,
                read_dependencies: Vec::new(),
            },
            scope: WorkScopeDefinition {
                workspace_id: WorkspaceId(Uuid::new_v4()),
                task_id: TaskId(Uuid::new_v4()),
                checkout_id: self.checkout_id,
                grant_id: Uuid::new_v4(),
                grant_revision: 1,
                scope_id: Uuid::new_v4(),
                revision: 1,
                operation: WorkScopeOperation::Apply,
                access_request_id: None,
                worker: Recipient {
                    user_id: UserId(Uuid::new_v4()),
                    agent_id: Some(AgentId(Uuid::new_v4())),
                },
                expires_at: Utc::now() + Duration::minutes(2),
                required_read_paths: Vec::new(),
                permissions: WorkPermissions {
                    read: allow_tree.clone(),
                    write: allow_tree,
                    write_operations: vec![
                        WriteOperation::Create,
                        WriteOperation::Rename,
                        WriteOperation::Replace,
                        WriteOperation::Delete,
                    ],
                    ..WorkPermissions::default()
                },
            },
        }
    }

    fn text(&self, path: &str) -> String {
        fs::read_to_string(self.tree.join(path)).unwrap()
    }
}

use std::os::unix::fs::DirBuilderExt;

fn sha(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn file(text: &str) -> CheckoutContent {
    CheckoutContent {
        encoding: FileEncoding::Utf8,
        content: text.into(),
    }
}

fn replace(path: &str, before: &str, after: &str) -> CheckoutChange {
    CheckoutChange::Replace {
        path: path.parse().unwrap(),
        before_sha256: sha(before),
        file: file(after),
        executable: None,
    }
}

fn deadline() -> Instant {
    Instant::now() + std::time::Duration::from_secs(25)
}

#[test]
fn finishes_two_file_transaction_after_interruption_and_replays_the_receipt() {
    let fixture = Fixture::new();
    let order = fixture.order(
        &fixture.head,
        vec![
            replace("alpha.txt", "alpha before", "alpha after"),
            replace("beta.txt", "beta before", "beta after"),
        ],
    );
    let state_id = {
        let local = fixture.lock();
        let publisher = fixture.publisher(&local);
        let journal = publisher.prepare(&order, deadline()).unwrap();
        publisher
            .apply_step(journal.change_id, 0, &journal.steps[0])
            .unwrap();
        publisher.state_id()
    };
    assert_eq!(fixture.text("alpha.txt"), "alpha after");
    assert_eq!(fixture.text("beta.txt"), "beta before");
    let local = fixture.lock();
    let publisher = fixture.publisher(&local);
    assert_eq!(publisher.state_id(), state_id);
    let outcome = publisher
        .recover(&fixture.head, Some(&order))
        .unwrap()
        .unwrap();
    let CheckoutWriteOutcome::Applied { head, files } = &outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(head.checkout_revision, 1);
    assert_eq!(files.len(), 2);
    assert_eq!(
        files[0].before_sha256.as_deref(),
        Some(sha("alpha before").as_str())
    );
    assert_eq!(
        files[1].after_sha256.as_deref(),
        Some(sha("beta after").as_str())
    );
    assert_eq!(fixture.text("beta.txt"), "beta after");
    assert_eq!(
        publisher.recover(&fixture.head, Some(&order)).unwrap(),
        Some(outcome.clone())
    );
    assert_eq!(publisher.recover(head, None).unwrap(), None);
    assert!(publisher.pending().unwrap().is_none());
    assert!(fixture.state.join(receipt_name(order.change_id)).is_file());
}

#[test]
fn recovery_preserves_unexpected_content_and_completed_changes() {
    let fixture = Fixture::new();
    let local = fixture.lock();
    let publisher = fixture.publisher(&local);
    let prior = fixture.order(
        &fixture.head,
        vec![CheckoutChange::Create {
            path: "accepted.txt".parse().unwrap(),
            file: file("accepted from first worker"),
            executable: false,
        }],
    );
    let CheckoutWriteOutcome::Applied { head, .. } = publisher.apply(&prior, deadline()).unwrap()
    else {
        panic!()
    };
    publisher.acknowledge(prior.change_id).unwrap();
    let order = fixture.order(
        &head,
        vec![
            replace("alpha.txt", "alpha before", "alpha after"),
            replace("beta.txt", "beta before", "beta after"),
        ],
    );
    let journal = publisher.prepare(&order, deadline()).unwrap();
    publisher
        .apply_step(journal.change_id, 0, &journal.steps[0])
        .unwrap();
    fs::write(fixture.tree.join("beta.txt"), "unexpected content").unwrap();
    drop(local);
    let local = fixture.lock();
    let publisher = fixture.publisher(&local);
    assert!(matches!(
        publisher.recover(&head, Some(&order)),
        Err(PublicationStateError::Reconcile)
    ));
    assert_eq!(fixture.text("accepted.txt"), "accepted from first worker");
    assert_eq!(fixture.text("alpha.txt"), "alpha after");
    assert_eq!(fixture.text("beta.txt"), "unexpected content");
    assert!(!fixture.state.join(receipt_name(order.change_id)).exists());
}

#[test]
fn supports_rename_delete_and_unrelated_prior_revisions_with_exact_hashes() {
    let fixture = Fixture::new();
    let local = fixture.lock();
    let publisher = fixture.publisher(&local);
    let renamed = fixture.order(
        &fixture.head,
        vec![CheckoutChange::Rename {
            source: "alpha.txt".parse().unwrap(),
            target: "another.txt".parse().unwrap(),
            before_sha256: sha("alpha before"),
        }],
    );
    let CheckoutWriteOutcome::Applied { head, files } =
        publisher.apply(&renamed, deadline()).unwrap()
    else {
        panic!()
    };
    assert!(!fixture.tree.join("alpha.txt").exists());
    assert_eq!(fixture.text("another.txt"), "alpha before");
    assert_eq!(files[0].after_sha256, None);
    assert_eq!(files[1].before_sha256, None);
    publisher.acknowledge(renamed.change_id).unwrap();
    let mut order = fixture.order(
        &head,
        vec![CheckoutChange::Delete {
            path: "beta.txt".parse().unwrap(),
            before_sha256: sha("beta before"),
        }],
    );
    order.input.read_dependencies.push(CheckoutDependency {
        path: "another.txt".parse().unwrap(),
        sha256: sha("old context"),
    });
    assert_eq!(
        publisher.apply(&order, deadline()).unwrap(),
        rejected(CheckoutPublicationError::ContextStale)
    );
    assert_eq!(fixture.text("beta.txt"), "beta before");
    order.input.read_dependencies[0].sha256 = sha("alpha before");
    assert_eq!(order.input.base_checkout_revision, 0);
    let CheckoutWriteOutcome::Applied { head, .. } = publisher.apply(&order, deadline()).unwrap()
    else {
        panic!()
    };
    assert_eq!(head.checkout_revision, 2);
    assert!(!fixture.tree.join("beta.txt").exists());
}

#[test]
fn rejects_stale_hashes_expired_grants_and_links_before_changes() {
    let fixture = Fixture::new();
    let local = fixture.lock();
    let publisher = fixture.publisher(&local);
    let order = fixture.order(&fixture.head, vec![replace("alpha.txt", "wrong", "after")]);
    assert_eq!(
        publisher.apply(&order, deadline()).unwrap(),
        rejected(CheckoutPublicationError::ContentConflict)
    );
    assert_eq!(
        publisher.apply(&order, Instant::now()).unwrap(),
        rejected(CheckoutPublicationError::InterruptedBeforeStart)
    );
    let target = fixture.tree.join("alpha.txt");
    symlink(&target, fixture.tree.join("link.txt")).unwrap();
    let link = fixture.order(
        &fixture.head,
        vec![replace("link.txt", "alpha before", "after")],
    );
    assert_eq!(
        publisher.apply(&link, deadline()).unwrap(),
        rejected(CheckoutPublicationError::Forbidden)
    );
    fs::hard_link(&target, fixture.tree.join("linked.txt")).unwrap();
    let file = fixture.order(
        &fixture.head,
        vec![replace("alpha.txt", "alpha before", "after")],
    );
    assert_eq!(
        publisher.apply(&file, deadline()).unwrap(),
        rejected(CheckoutPublicationError::Forbidden)
    );
    assert_eq!(fixture.text("alpha.txt"), "alpha before");
    assert!(publisher.pending().unwrap().is_none());
}

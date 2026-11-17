use office_checkout::check_local_checkout;
use office_domain::TaskId;
use office_protocol::{CheckoutHostHealth, DoctorReport, InspectCheckoutHost};

use crate::{
    CheckoutDoctorOptions, service,
    transport::{client, confidential_url, endpoint, print_json, read_token, request},
};

pub(super) async fn run(options: CheckoutDoctorOptions, json: bool) -> Result<(), String> {
    let mut report = DoctorReport::default();
    match authority(&options).await {
        Ok(authority) => {
            report.add(
                "host_authority",
                Ok(format!("Registered checkout {}", authority.checkout_id)),
            );
            check_local_checkout(
                &options.root,
                &options.state_dir,
                &authority,
                options.scope_task.is_some(),
                &mut report,
            );
            match (&options.probe_image, report.passed()) {
                (Some(image), true) => report.add(
                    "native_write_denial",
                    service::probe_checkout_isolation(&options.state_dir, &options.root, image)
                        .await,
                ),
                (Some(_), false) => report.skip(
                    "native_write_denial",
                    "A verified checkout and journal are required",
                ),
                (None, _) => report.skip(
                    "native_write_denial",
                    "Pass --probe-image to run the isolated native write probe",
                ),
            }
            if report.passed() {
                report.add("consistency", still_current(&options, &authority).await);
            }
        }
        Err(error) => report.add("host_authority", Err(error)),
    }
    let passed = report.passed();
    if json {
        print_json(&report)?;
    } else {
        for check in report.checks {
            println!(
                "{:?} · {} · {}",
                check.status, check.component, check.detail
            );
        }
    }
    if passed {
        Ok(())
    } else {
        Err("Checkout diagnostics failed".into())
    }
}

async fn authority(options: &CheckoutDoctorOptions) -> Result<CheckoutHostHealth, String> {
    let url = confidential_url(&options.connection.url)?;
    let token = read_token(&options.connection.token_file)?;
    let target = endpoint(&url, "v1/runner-checkouts/diagnostics")?;
    let health: CheckoutHostHealth = request(
        client()?.post(target).json(&InspectCheckoutHost {
            checkout_id: options.checkout,
            scope_task_id: options.scope_task.map(TaskId),
        }),
        &token,
    )
    .await?;
    if health.checkout_id != options.checkout {
        return Err("Authority returned a different checkout".into());
    }
    Ok(health)
}

async fn still_current(
    options: &CheckoutDoctorOptions,
    initial: &CheckoutHostHealth,
) -> Result<String, String> {
    let observed = authority(options).await?;
    let epoch = |report: &CheckoutHostHealth| report.lease.as_ref().map(|lease| lease.epoch);
    let scope = |report: &CheckoutHostHealth| {
        report
            .scope
            .as_ref()
            .map(|scope| (scope.scope_id, scope.revision))
    };
    if epoch(&observed) != epoch(initial)
        || observed.head != initial.head
        || scope(&observed) != scope(initial)
        || observed.applies_pending
        || observed.needs_reconcile
    {
        return Err(
            "The checkout changed during diagnostics. Repeat after publication or recovery".into(),
        );
    }
    Ok("Coordinator epoch, head and scope verified against the server after the probe".into())
}

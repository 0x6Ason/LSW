// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::OsString;

use lsw_core::{CustomizationPlan, WindowsProfile};

pub(super) fn command(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let profile = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw profile PROFILE")?
        .parse::<WindowsProfile>()?;
    if arguments.len() != 1 {
        return Err("usage: lsw profile PROFILE".into());
    }
    let plan = CustomizationPlan::for_profile(profile)?;
    println!("LSW Windows profile: {}", plan.profile);
    println!("  profile revision: {}", plan.revision);
    println!("  servicing preserved: {}", profile.keeps_servicing());
    println!("  CompactOS requested: {}", plan.compact_os);
    if plan.appx_removals.is_empty() {
        println!("  exact AppX removals: none");
    } else {
        println!("  exact AppX identities removed locally:");
        for removal in plan.appx_removals {
            println!("    - {}", removal.display_name);
        }
    }
    print_counted_list(
        "optional-feature payload removals",
        &plan.optional_feature_removals,
    );
    println!(
        "  supported product uninstallers: {}",
        plan.product_uninstallers.len()
    );
    println!(
        "  service startup policies: {}",
        plan.service_policies.len()
    );
    println!("  registry policies: {}", plan.registry_policies.len());
    if matches!(profile, WindowsProfile::Slim | WindowsProfile::Ephemeral) {
        println!("  audit: C:\\ProgramData\\LSW\\profile\\report.json");
    }
    println!("  explicitly preserved:");
    for component in plan.preserve_components {
        println!("    - {component}");
    }
    for warning in plan.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}

fn print_counted_list(label: &str, values: &[String]) {
    if values.is_empty() {
        println!("  {label}: none");
    } else {
        println!("  {label}: {}", values.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_or_extra_profile_arguments() {
        assert!(command(&[]).is_err());
        assert!(command(&["slim".into(), "extra".into()]).is_err());
    }
}

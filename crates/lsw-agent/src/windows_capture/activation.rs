// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(crate) fn find_process_window(
    process_id: u32,
    existing_windows: &BTreeSet<isize>,
    child: &mut Child,
) -> Result<(WindowHandle, u32), Box<dyn std::error::Error>> {
    let launcher_handle = HANDLE(child.as_raw_handle() as isize);
    let launcher = ProcessKey {
        pid: process_id,
        creation_time: process_creation_time(launcher_handle)?,
    };
    let session_id = process_session_id(std::process::id())?;
    let deadline = Instant::now() + WINDOW_DISCOVERY_TIMEOUT;
    let mut launcher_status = None;
    let mut stable_exact = CandidateStability::default();
    loop {
        let candidates = enumerate_process_windows(existing_windows)?;
        match select_exact_window_candidate(&candidates, launcher, session_id, false, None) {
            Some(candidate) => {
                if let Some(candidate) = stable_exact.observe(candidate) {
                    return open_selected_window(candidate, launcher);
                }
            }
            None => stable_exact.reset(),
        }
        if launcher_status.is_none() {
            launcher_status = child.try_wait()?;
        }
        if Instant::now() >= deadline {
            return if let Some(status) = launcher_status {
                Err(format!(
                    "GUI launcher exited with {status} without creating a unique visible top-level window"
                )
                .into())
            } else {
                Err("timed out waiting for the GUI process to create a visible window".into())
            };
        }
        thread::sleep(WINDOW_DISCOVERY_INTERVAL);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActivatedPackageIdentity {
    pub(crate) package_full_name: String,
    pub(crate) package_family_name: String,
    pub(crate) aumid: String,
}

pub(crate) struct ActivatedApplication {
    owner: OwnedProcess,
    process: ProcessKey,
    session_id: u32,
    package: ActivatedPackageIdentity,
    activation_filetime: u64,
}

impl ActivatedApplication {
    fn validate(&self) -> windows::core::Result<()> {
        // SAFETY: owner.handle pins the exact process object returned by AAM
        // and includes SYNCHRONIZE for the lifetime of this activation.
        if unsafe { WaitForSingleObject(self.owner.handle, 0) } != WAIT_TIMEOUT
            || process_creation_time(self.owner.handle).ok() != Some(self.process.creation_time)
            || self.process.creation_time < self.activation_filetime
        {
            return Err(Error::new(
                INVALID_ARGUMENT,
                "AAM-activated GUI process is no longer the pinned launch result",
            ));
        }
        Ok(())
    }
}

pub(crate) fn activate_packaged_alias(
    requested_executable: &str,
) -> Result<Option<ActivatedApplication>, Box<dyn std::error::Error>> {
    let Some(expected_alias) = packaged_activation_alias_name(requested_executable)? else {
        return Ok(None);
    };
    let _apartment = ApartmentGuard::initialize()?;
    let Some(package) = resolve_current_user_packaged_alias(&expected_alias)? else {
        return Ok(None);
    };
    // SAFETY: the calling thread has an active WinRT/COM apartment. The class
    // and interface are the documented ApplicationActivationManager pair.
    let manager: IApplicationActivationManager =
        unsafe { CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_INPROC_SERVER)? };
    let aumid = nul_terminated_utf16(&package.aumid, "application user model ID")?;
    let empty = [0_u16];
    // Snapshot and timestamp only after every fallible activation prerequisite
    // is ready, leaving no package scan, COM creation, or string-allocation gap
    // in which an unrelated singleton could start and look launch-correlated.
    let pre_activation_processes = process_id_snapshot()?;
    // SAFETY: this reads the precise system FILETIME directly before the
    // documented activation call. Process creation times use the same epoch.
    let activation_filetime = filetime_value(unsafe { GetSystemTimePreciseAsFileTime() });
    // SAFETY: both PCWSTR values are NUL-terminated and live for the complete
    // synchronous activation. AO_NOERRORUI suppresses only OS activation error
    // UI. AO_NOSPLASHSCREEN is deliberately not used: Microsoft documents it
    // for debug-enabled packages and PLM may otherwise terminate a retail app.
    let pid = unsafe {
        manager.ActivateApplication(PCWSTR(aumid.as_ptr()), PCWSTR(empty.as_ptr()), AO_NOERRORUI)?
    };
    if pid == 0 {
        return Err("ApplicationActivationManager returned a zero PID".into());
    }
    let owner = OwnedProcess::open(pid)?;
    if !activation_process_is_new(
        pid,
        owner.creation_time,
        activation_filetime,
        &pre_activation_processes,
    ) {
        return Err(
            "packaged GUI activation reused an existing/singleton process; refusing ambiguous HWND correlation"
                .into(),
        );
    }
    let session_id = process_session_id(pid)?;
    if session_id != process_session_id(std::process::id())? {
        return Err("AAM activated the GUI application in a different Windows session".into());
    }
    validate_process_package_identity(owner.handle, &package)?;
    Ok(Some(ActivatedApplication {
        process: ProcessKey {
            pid,
            creation_time: owner.creation_time,
        },
        owner,
        session_id,
        package,
        activation_filetime,
    }))
}

pub(crate) fn find_activated_window(
    activation: ActivatedApplication,
    existing_windows: &BTreeSet<isize>,
) -> Result<(WindowHandle, u32), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + WINDOW_DISCOVERY_TIMEOUT;
    let mut stability = CandidateStability::default();
    loop {
        activation.validate()?;
        let candidates = enumerate_process_windows(existing_windows)?;
        match select_exact_window_candidate(
            &candidates,
            activation.process,
            activation.session_id,
            true,
            Some(&activation.package),
        ) {
            Some(candidate) => {
                if let Some(candidate) = stability.observe(candidate) {
                    activation.validate()?;
                    let selected = open_selected_window(candidate, activation.process)?;
                    drop(activation);
                    return Ok(selected);
                }
            }
            None => stability.reset(),
        }
        if Instant::now() >= deadline {
            return Err(
                "AAM activation did not create one unique stable visible HWND owned by its returned PID"
                    .into(),
            );
        }
        thread::sleep(WINDOW_DISCOVERY_INTERVAL);
    }
}

pub(crate) fn resolve_current_user_packaged_alias(
    expected_alias: &str,
) -> Result<Option<ActivatedPackageIdentity>, Box<dyn std::error::Error>> {
    // Empty SID is the documented current-user form. FindPackages() without a
    // SID spans all users and would violate the interactive companion scope.
    let packages = PackageManager::new()?.FindPackagesByUserSecurityId(&HSTRING::new())?;
    let iterator = packages.First()?;
    let mut scanned = 0_usize;
    let mut matches = Vec::new();
    while iterator.HasCurrent()? {
        scanned += 1;
        if scanned > MAX_CURRENT_USER_PACKAGES {
            return Err(format!(
                "current-user package scan exceeds the {MAX_CURRENT_USER_PACKAGES} package safety limit"
            )
            .into());
        }
        let package = iterator.Current()?;
        let id = package.Id()?;
        let package_full_name = strict_hstring(id.FullName()?, "package full name")?;
        let package_family_name = strict_hstring(id.FamilyName()?, "package family name")?;
        if package_full_name.is_empty() || package_family_name.is_empty() {
            return Err("PackageManager returned an empty package identity".into());
        }
        if let Some(application_id) =
            package_manifest_alias_application(&package_full_name, expected_alias)?
        {
            if application_id.contains(['!', '\0']) {
                return Err("package manifest application ID is invalid for an AUMID".into());
            }
            let aumid = format!("{package_family_name}!{application_id}");
            validate_aumid(&aumid)?;
            matches.push(ActivatedPackageIdentity {
                package_full_name,
                package_family_name,
                aumid,
            });
            if matches.len() > 1 {
                return Err(format!(
                    "AppExecutionAlias {expected_alias:?} is declared by multiple current-user packages"
                )
                .into());
            }
        }
        iterator.MoveNext()?;
    }
    Ok(matches.pop())
}

pub(crate) fn validate_process_package_identity(
    handle: HANDLE,
    expected: &ActivatedPackageIdentity,
) -> Result<(), Box<dyn std::error::Error>> {
    let full_name = process_package_full_name(handle)?
        .ok_or("AAM returned an unpackaged process for a packaged alias")?;
    let family_name = process_package_family_name(handle)?
        .ok_or("AAM returned a process without a package family")?;
    let aumid = process_application_user_model_id(handle)?
        .ok_or("AAM returned a process without an application user model ID")?;
    if !windows_ordinal_eq_ignore_case(&full_name, &expected.package_full_name)
        || !windows_ordinal_eq_ignore_case(&family_name, &expected.package_family_name)
        || !windows_ordinal_eq_ignore_case(&aumid, &expected.aumid)
    {
        return Err("AAM returned PID package/AUMID does not match the resolved alias".into());
    }
    Ok(())
}

pub(crate) fn activation_process_is_new(
    pid: u32,
    creation_time: u64,
    activation_filetime: u64,
    pre_activation_processes: &BTreeSet<u32>,
) -> bool {
    pid != 0 && !pre_activation_processes.contains(&pid) && creation_time >= activation_filetime
}

pub(crate) struct ProcessSnapshotHandle(HANDLE);

impl Drop for ProcessSnapshotHandle {
    fn drop(&mut self) {
        // SAFETY: this value exclusively owns the Toolhelp snapshot handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(crate) fn process_id_snapshot() -> Result<BTreeSet<u32>, Box<dyn std::error::Error>> {
    let snapshot = ProcessSnapshotHandle(
        // SAFETY: TH32CS_SNAPPROCESS with PID zero creates a caller-owned snapshot
        // of system process identifiers and does not inspect process memory.
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? },
    );
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>())?,
        ..Default::default()
    };
    // SAFETY: entry has the required dwSize and remains writable for iteration.
    unsafe { Process32FirstW(snapshot.0, &mut entry)? };
    let mut processes = BTreeSet::new();
    loop {
        if entry.th32ProcessID != 0 {
            processes.insert(entry.th32ProcessID);
            if processes.len() > MAX_PROCESS_SNAPSHOT_ENTRIES {
                return Err("process snapshot exceeds the safety entry limit".into());
            }
        }
        // SAFETY: snapshot and entry remain valid for the next synchronous
        // Toolhelp iteration call.
        match unsafe { Process32NextW(snapshot.0, &mut entry) } {
            Ok(()) => {}
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => {
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(processes)
}

pub(crate) fn filetime_value(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

pub(crate) fn validate_aumid(value: &str) -> Result<(), Box<dyn std::error::Error>> {
    let units = value.encode_utf16().count().saturating_add(1);
    if aumid_parts(value).is_none()
        || units == 1
        || units > usize::try_from(APPLICATION_USER_MODEL_ID_MAX_LENGTH)?
    {
        return Err("resolved package application user model ID is invalid".into());
    }
    Ok(())
}

pub(crate) fn nul_terminated_utf16(
    value: &str,
    field: &str,
) -> Result<Vec<u16>, Box<dyn std::error::Error>> {
    let mut units = value.encode_utf16().collect::<Vec<_>>();
    if units.is_empty() || units.contains(&0) {
        return Err(format!("{field} is empty or contains NUL").into());
    }
    units.push(0);
    Ok(units)
}

pub(crate) fn strict_hstring(
    value: HSTRING,
    field: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let units = value.as_wide();
    if units.is_empty() || units.contains(&0) {
        return Err(format!("PackageManager returned an invalid {field}").into());
    }
    Ok(String::from_utf16(units)?)
}

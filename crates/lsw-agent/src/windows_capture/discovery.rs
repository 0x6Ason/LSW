// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessKey {
    pub(crate) pid: u32,
    pub(crate) creation_time: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WindowCandidate {
    pub(crate) hwnd: isize,
    pub(crate) pid: u32,
    pub(crate) creation_time: u64,
    pub(crate) session_id: u32,
    pub(crate) image_path: Option<String>,
    pub(crate) package_full_name: Option<String>,
    pub(crate) package_family_name: Option<String>,
    pub(crate) application_user_model_id: Option<String>,
    pub(crate) new_window: bool,
}

#[derive(Default)]
pub(crate) struct CandidateStability {
    pub(crate) previous: Option<WindowCandidate>,
}

impl CandidateStability {
    pub(crate) fn observe(&mut self, candidate: WindowCandidate) -> Option<WindowCandidate> {
        if self.previous.as_ref() == Some(&candidate) {
            self.previous = None;
            Some(candidate)
        } else {
            self.previous = Some(candidate);
            None
        }
    }

    pub(crate) fn reset(&mut self) {
        self.previous = None;
    }
}

pub(crate) struct WindowEnumeration {
    windows: Vec<(isize, u32)>,
}

pub(crate) fn visible_windows() -> windows::core::Result<BTreeSet<isize>> {
    let mut windows = BTreeSet::new();
    // SAFETY: LPARAM contains a valid BTreeSet pointer for this synchronous
    // EnumWindows invocation. The callback only inserts HWND integer values.
    unsafe {
        EnumWindows(
            Some(collect_visible_window),
            LPARAM((&mut windows as *mut BTreeSet<isize>) as isize),
        )?;
    }
    Ok(windows)
}

unsafe extern "system" fn collect_visible_window(hwnd: HWND, parameter: LPARAM) -> BOOL {
    // SAFETY: visible_windows passes this exact pointer and EnumWindows invokes
    // callbacks synchronously before the stack allocation is dropped.
    let windows = unsafe { &mut *(parameter.0 as *mut BTreeSet<isize>) };
    // SAFETY: IsWindowVisible accepts the HWND supplied by EnumWindows.
    if unsafe { IsWindowVisible(hwnd).as_bool() } {
        windows.insert(hwnd.0);
    }
    BOOL(1)
}

pub(crate) fn enumerate_process_windows(
    existing_windows: &BTreeSet<isize>,
) -> windows::core::Result<Vec<WindowCandidate>> {
    let mut enumeration = WindowEnumeration {
        windows: Vec::new(),
    };
    // SAFETY: LPARAM contains a valid WindowEnumeration pointer for this
    // synchronous EnumWindows invocation. The callback stores only integer
    // HWND/PID pairs and does not retain the pointer.
    unsafe {
        EnumWindows(
            Some(enumerate_visible_window),
            LPARAM((&mut enumeration as *mut WindowEnumeration) as isize),
        )?;
    }
    let mut candidates = Vec::new();
    for (hwnd, pid) in enumeration.windows {
        let Ok(process) = ProcessSnapshot::open(pid) else {
            continue;
        };
        candidates.push(WindowCandidate {
            hwnd,
            pid,
            creation_time: process.creation_time,
            session_id: process.session_id,
            image_path: process.image_path,
            package_full_name: process.package_full_name,
            package_family_name: process.package_family_name,
            application_user_model_id: process.application_user_model_id,
            new_window: !existing_windows.contains(&hwnd),
        });
    }
    Ok(candidates)
}

unsafe extern "system" fn enumerate_visible_window(hwnd: HWND, parameter: LPARAM) -> BOOL {
    // SAFETY: enumerate_process_windows passes this exact pointer and
    // EnumWindows invokes callbacks synchronously before the stack allocation
    // is dropped.
    let enumeration = unsafe { &mut *(parameter.0 as *mut WindowEnumeration) };
    // SAFETY: All queried functions accept an HWND supplied by EnumWindows and
    // the process-id output points at live stack storage.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut observed = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut observed));
        if observed != 0 {
            enumeration.windows.push((hwnd.0, observed));
        }
    }
    BOOL(1)
}

pub(crate) fn select_exact_window_candidate(
    candidates: &[WindowCandidate],
    expected: ProcessKey,
    session_id: u32,
    require_new_window: bool,
    package_identity: Option<&ActivatedPackageIdentity>,
) -> Option<WindowCandidate> {
    let exact = candidates
        .iter()
        .filter(|candidate| {
            candidate.pid == expected.pid
                && candidate.creation_time == expected.creation_time
                && candidate.session_id == session_id
                && (!require_new_window || candidate.new_window)
                && package_identity.map_or(true, |identity| {
                    candidate.package_full_name.as_deref() == Some(&identity.package_full_name)
                        && candidate.package_family_name.as_deref()
                            == Some(&identity.package_family_name)
                        && candidate.application_user_model_id.as_deref() == Some(&identity.aumid)
                })
        })
        .collect::<Vec<_>>();
    match exact.as_slice() {
        [candidate] => Some((*candidate).clone()),
        // Splash/main coexistence can be transient. Wait for one unique HWND
        // and then require it to remain stable on the next poll. Ambiguity
        // never permits a temporal or other-PID fallback.
        _ => None,
    }
}

pub(crate) fn open_selected_window(
    candidate: WindowCandidate,
    expected: ProcessKey,
) -> Result<(WindowHandle, u32), Box<dyn std::error::Error>> {
    let owner = OwnedProcess::open(candidate.pid)?;
    if owner.pid != expected.pid
        || owner.creation_time != expected.creation_time
        || owner.creation_time != candidate.creation_time
    {
        return Err("GUI window owner changed identity during discovery".into());
    }
    let hwnd = HWND(candidate.hwnd);
    if !window_owner_matches(hwnd, owner.pid) {
        return Err("GUI HWND owner changed during discovery".into());
    }
    let owner_pid = owner.pid;
    Ok((
        WindowHandle {
            hwnd,
            owner,
            capture_size: (0, 0),
            injected: InjectedInputState::default(),
        },
        owner_pid,
    ))
}

pub(crate) struct ProcessSnapshot {
    creation_time: u64,
    session_id: u32,
    image_path: Option<String>,
    package_full_name: Option<String>,
    package_family_name: Option<String>,
    application_user_model_id: Option<String>,
}

impl ProcessSnapshot {
    pub(crate) fn open(pid: u32) -> windows::core::Result<Self> {
        let process = OwnedProcess::open(pid)?;
        let image_path = process_image_path(process.handle).ok();
        let package_full_name = process_package_full_name(process.handle).ok().flatten();
        let package_family_name = process_package_family_name(process.handle).ok().flatten();
        let application_user_model_id = process_application_user_model_id(process.handle)
            .ok()
            .flatten();
        Ok(Self {
            creation_time: process.creation_time,
            session_id: process_session_id(pid)?,
            image_path,
            package_full_name,
            package_family_name,
            application_user_model_id,
        })
    }
}

impl OwnedProcess {
    pub(crate) fn open(pid: u32) -> windows::core::Result<Self> {
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
        // SAFETY: OpenProcess receives a numeric PID and returns a caller-owned
        // query/synchronization handle. GUI teardown never requests terminate
        // rights or force-kills an application with potentially unsaved data.
        let handle = unsafe { OpenProcess(access, false, pid)? };
        let mut process = Self {
            handle,
            pid,
            creation_time: 0,
        };
        process.creation_time = process_creation_time(process.handle)?;
        Ok(process)
    }
}

pub(crate) fn process_creation_time(handle: HANDLE) -> windows::core::Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all output pointers refer to initialized writable FILETIME values
    // and handle has PROCESS_QUERY_LIMITED_INFORMATION.
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)? };
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

pub(crate) fn process_session_id(pid: u32) -> windows::core::Result<u32> {
    let mut session = 0_u32;
    // SAFETY: session points at initialized writable storage and PID is a
    // numeric process identifier obtained from Windows.
    unsafe { ProcessIdToSessionId(pid, &mut session)? };
    Ok(session)
}

pub(crate) fn process_image_path(handle: HANDLE) -> Result<String, Box<dyn std::error::Error>> {
    let mut buffer = vec![0_u16; MAX_PROCESS_IMAGE_UNITS];
    let mut length = u32::try_from(buffer.len())?;
    // SAFETY: buffer is writable for length UTF-16 units, length remains valid,
    // and the process handle has limited query permission.
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )?
    };
    buffer.truncate(usize::try_from(length)?);
    let image = String::from_utf16(&buffer)?;
    if image.is_empty() {
        return Err("GUI process image path is empty".into());
    }
    Ok(image)
}

pub(crate) fn process_package_full_name(
    handle: HANDLE,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut length = 0_u32;
    // SAFETY: the first call is the documented size query. The process handle
    // has limited query permission and no output buffer is provided.
    let status = unsafe { GetPackageFullName(handle, &mut length, PWSTR::null()) };
    if status == APPMODEL_ERROR_NO_PACKAGE {
        return Ok(None);
    }
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(win32_status_error(
            "GetPackageFullName size query",
            status.0,
        ));
    }
    let length = usize::try_from(length)?;
    if length == 0 || length > MAX_PACKAGE_NAME_UNITS {
        return Err("Windows returned an invalid package full-name length".into());
    }
    let mut buffer = vec![0_u16; length];
    let mut written = u32::try_from(buffer.len())?;
    // SAFETY: buffer is writable for written UTF-16 units and the process
    // handle remains live for this synchronous query.
    let status = unsafe { GetPackageFullName(handle, &mut written, PWSTR(buffer.as_mut_ptr())) };
    if status != ERROR_SUCCESS {
        return Err(win32_status_error("GetPackageFullName", status.0));
    }
    let package = String::from_utf16(validated_nul_terminated_units(
        &buffer,
        written,
        "package full name",
    )?)?;
    if package.is_empty() {
        return Err("Windows returned an empty package full name".into());
    }
    Ok(Some(package))
}

pub(crate) fn process_package_family_name(
    handle: HANDLE,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut length = 0_u32;
    // SAFETY: this is the documented size query and handle has limited process
    // query permission. No output buffer is supplied on the first call.
    let status = unsafe { GetPackageFamilyName(handle, &mut length, PWSTR::null()) };
    if status == APPMODEL_ERROR_NO_PACKAGE {
        return Ok(None);
    }
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(win32_status_error(
            "GetPackageFamilyName size query",
            status.0,
        ));
    }
    if length == 0 || length > PACKAGE_FAMILY_NAME_MAX_LENGTH + 1 {
        return Err("Windows returned an invalid package family-name length".into());
    }
    let mut buffer = vec![0_u16; usize::try_from(length)?];
    let mut written = u32::try_from(buffer.len())?;
    // SAFETY: buffer is writable for written UTF-16 units and handle remains
    // live with limited query permission for this synchronous call.
    let status = unsafe { GetPackageFamilyName(handle, &mut written, PWSTR(buffer.as_mut_ptr())) };
    if status != ERROR_SUCCESS {
        return Err(win32_status_error("GetPackageFamilyName", status.0));
    }
    let family = String::from_utf16(validated_nul_terminated_units(
        &buffer,
        written,
        "package family name",
    )?)?;
    if family.is_empty() {
        return Err("Windows returned an empty package family name".into());
    }
    Ok(Some(family))
}

pub(crate) fn process_application_user_model_id(
    handle: HANDLE,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut length = 0_u32;
    // SAFETY: this is the documented size query and handle has limited process
    // query permission. No output buffer is supplied on the first call.
    let status = unsafe { GetApplicationUserModelId(handle, &mut length, PWSTR::null()) };
    if status == APPMODEL_ERROR_NO_APPLICATION {
        return Ok(None);
    }
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(win32_status_error(
            "GetApplicationUserModelId size query",
            status.0,
        ));
    }
    if length == 0 || length > APPLICATION_USER_MODEL_ID_MAX_LENGTH {
        return Err("Windows returned an invalid application user model ID length".into());
    }
    let mut buffer = vec![0_u16; usize::try_from(length)?];
    let mut written = u32::try_from(buffer.len())?;
    // SAFETY: buffer is writable for written UTF-16 units and handle remains
    // live with limited query permission for this synchronous call.
    let status =
        unsafe { GetApplicationUserModelId(handle, &mut written, PWSTR(buffer.as_mut_ptr())) };
    if status != ERROR_SUCCESS {
        return Err(win32_status_error("GetApplicationUserModelId", status.0));
    }
    let aumid = String::from_utf16(validated_nul_terminated_units(
        &buffer,
        written,
        "application user model ID",
    )?)?;
    if aumid_parts(&aumid).is_none() {
        return Err("Windows returned an invalid application user model ID".into());
    }
    Ok(Some(aumid))
}

pub(crate) fn package_manifest_alias_application(
    package_full_name: &str,
    expected_alias: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let package_path = package_path_by_full_name(package_full_name)?;
    let manifest_path = package_path.join("AppxManifest.xml");
    let file = File::open(manifest_path)?;
    if file.metadata()?.len() > MAX_PACKAGE_MANIFEST_BYTES {
        return Err(format!(
            "package manifest exceeds the {} byte correlation limit",
            MAX_PACKAGE_MANIFEST_BYTES
        )
        .into());
    }
    let mut bytes = Vec::new();
    BufReader::new(file)
        .take(MAX_PACKAGE_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len())? > MAX_PACKAGE_MANIFEST_BYTES {
        return Err("package manifest grew beyond the correlation limit while reading".into());
    }
    manifest_xml_alias_application(&bytes, expected_alias)
}

pub(crate) fn package_path_by_full_name(
    package_full_name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let package_wide = package_full_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let package = PCWSTR(package_wide.as_ptr());
    let mut length = 0_u32;
    // SAFETY: package points at a NUL-terminated UTF-16 package full name for
    // both synchronous calls. The first call is the documented size query.
    let status = unsafe { GetPackagePathByFullName(package, &mut length, PWSTR::null()) };
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(win32_status_error(
            "GetPackagePathByFullName size query",
            status.0,
        ));
    }
    let length = usize::try_from(length)?;
    if length == 0 || length > MAX_PACKAGE_PATH_UNITS {
        return Err("Windows returned an invalid package path length".into());
    }
    let mut buffer = vec![0_u16; length];
    let mut written = u32::try_from(buffer.len())?;
    // SAFETY: the output buffer is writable for written UTF-16 units and the
    // input package name remains NUL terminated for the synchronous call.
    let status =
        unsafe { GetPackagePathByFullName(package, &mut written, PWSTR(buffer.as_mut_ptr())) };
    if status != ERROR_SUCCESS {
        return Err(win32_status_error("GetPackagePathByFullName", status.0));
    }
    let path_units = validated_nul_terminated_units(&buffer, written, "package path")?;
    if path_units.is_empty() {
        return Err("Windows returned an empty package path".into());
    }
    Ok(PathBuf::from(OsString::from_wide(path_units)))
}

pub(crate) fn validated_nul_terminated_units<'a>(
    buffer: &'a [u16],
    written: u32,
    field: &str,
) -> Result<&'a [u16], Box<dyn std::error::Error>> {
    let written = usize::try_from(written)?;
    if written == 0 || written > buffer.len() || buffer[written - 1] != 0 {
        return Err(format!("Windows returned an invalid {field} buffer").into());
    }
    let value = &buffer[..written - 1];
    if value.contains(&0) {
        return Err(format!("Windows returned an interior NUL in {field}").into());
    }
    Ok(value)
}

pub(crate) fn manifest_xml_alias_application(
    manifest: &[u8],
    expected_alias: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if u64::try_from(manifest.len())? > MAX_PACKAGE_MANIFEST_BYTES {
        return Err("package manifest exceeds the correlation parser limit".into());
    }
    let mut parser = ParserConfig::new()
        .trim_whitespace(true)
        .ignore_comments(true)
        .coalesce_characters(false)
        .allow_multiple_root_elements(false)
        .max_entity_expansion_length(64 * 1024)
        .max_entity_expansion_depth(4)
        .max_name_length(256)
        .max_attributes(64)
        .max_attribute_length(4096)
        .max_data_length(1024 * 1024)
        .create_reader(manifest);
    let mut depth = 0_usize;
    let mut package_root = None::<usize>;
    let mut applications = None::<usize>;
    let mut application = None::<(usize, String)>;
    let mut application_extensions = None::<usize>;
    let mut alias_extension = None::<(usize, String, AliasExtensionSchema)>;
    let mut alias_container = None::<(usize, String, AliasExtensionSchema)>;
    let mut matching_applications = BTreeSet::new();
    let mut event_index = 0_usize;
    loop {
        if event_index >= MAX_PACKAGE_MANIFEST_EVENTS {
            return Err("package manifest exceeds the XML event limit".into());
        }
        event_index += 1;
        let event = parser.next()?;
        let end_document = matches!(event, XmlEvent::EndDocument);
        match event {
            XmlEvent::StartElement {
                name, attributes, ..
            } => {
                depth = depth
                    .checked_add(1)
                    .ok_or("package manifest depth overflowed")?;
                if depth > MAX_PACKAGE_MANIFEST_DEPTH {
                    return Err("package manifest exceeds the XML depth limit".into());
                }
                if depth == 1
                    && name.local_name == "Package"
                    && name.namespace.as_deref() == Some(FOUNDATION_MANIFEST_NAMESPACE)
                {
                    package_root = Some(depth);
                } else if applications.is_none()
                    && package_root.is_some_and(|root_depth| depth == root_depth + 1)
                    && name.local_name == "Applications"
                    && name.namespace.as_deref() == Some(FOUNDATION_MANIFEST_NAMESPACE)
                {
                    applications = Some(depth);
                } else if application.is_none()
                    && applications
                        .is_some_and(|applications_depth| depth == applications_depth + 1)
                    && name.local_name == "Application"
                    && name.namespace.as_deref() == Some(FOUNDATION_MANIFEST_NAMESPACE)
                {
                    if let Some(application_id) = attributes
                        .iter()
                        .find(|attribute| {
                            attribute.name.local_name == "Id" && attribute.name.namespace.is_none()
                        })
                        .map(|attribute| attribute.value.clone())
                        .filter(|application_id| !application_id.is_empty())
                    {
                        application = Some((depth, application_id));
                    }
                } else if application_extensions.is_none()
                    && application
                        .as_ref()
                        .is_some_and(|(application_depth, _)| depth == application_depth + 1)
                    && name.local_name == "Extensions"
                    && name.namespace.as_deref() == Some(FOUNDATION_MANIFEST_NAMESPACE)
                {
                    application_extensions = Some(depth);
                } else if alias_extension.is_none()
                    && application_extensions
                        .is_some_and(|extensions_depth| depth == extensions_depth + 1)
                    && name.local_name == "Extension"
                    && alias_extension_schema(name.namespace.as_deref()).is_some()
                    && attributes.iter().any(|attribute| {
                        attribute.name.local_name == "Category"
                            && attribute.name.namespace.is_none()
                            && attribute
                                .value
                                .eq_ignore_ascii_case("windows.appExecutionAlias")
                    })
                {
                    if let Some((_, application_id)) = application.as_ref() {
                        alias_extension = Some((
                            depth,
                            application_id.clone(),
                            alias_extension_schema(name.namespace.as_deref())
                                .expect("schema checked above"),
                        ));
                    }
                } else if let Some((extension_depth, application_id, schema)) =
                    alias_extension.as_ref()
                {
                    if alias_container.is_none()
                        && depth == extension_depth + 1
                        && name.local_name == "AppExecutionAlias"
                        && name.namespace.as_deref() == Some(schema.extension_namespace())
                    {
                        alias_container = Some((depth, application_id.clone(), *schema));
                    }
                }
                if let Some((container_depth, application_id, schema)) = alias_container.as_ref() {
                    if depth == container_depth + 1
                        && name.local_name == "ExecutionAlias"
                        && schema.execution_namespace_allowed(name.namespace.as_deref())
                        && attributes.iter().any(|attribute| {
                            attribute.name.local_name == "Alias"
                                && attribute.name.namespace.is_none()
                                && windows_ordinal_eq_ignore_case(&attribute.value, expected_alias)
                        })
                    {
                        matching_applications.insert(application_id.clone());
                    }
                }
            }
            XmlEvent::EndElement { .. } => {
                if alias_container
                    .as_ref()
                    .is_some_and(|(container_depth, _, _)| *container_depth == depth)
                {
                    alias_container = None;
                }
                if alias_extension
                    .as_ref()
                    .is_some_and(|(extension_depth, _, _)| *extension_depth == depth)
                {
                    alias_extension = None;
                }
                if application_extensions.is_some_and(|extensions_depth| extensions_depth == depth)
                {
                    application_extensions = None;
                }
                if application
                    .as_ref()
                    .is_some_and(|(application_depth, _)| *application_depth == depth)
                {
                    application = None;
                }
                if applications.is_some_and(|applications_depth| applications_depth == depth) {
                    applications = None;
                }
                if package_root.is_some_and(|root_depth| root_depth == depth) {
                    package_root = None;
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or("package manifest ended outside its root element")?;
            }
            _ => {}
        }
        if end_document {
            break;
        }
    }
    if parser.doctype().is_some() {
        return Err("package manifest DOCTYPE is not accepted for GUI correlation".into());
    }
    match matching_applications.len() {
        0 => Ok(None),
        1 => Ok(matching_applications.into_iter().next()),
        count => Err(format!(
            "package manifest declares {expected_alias} for {count} applications"
        )
        .into()),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AliasExtensionSchema {
    Uap3,
    Uap5,
}

impl AliasExtensionSchema {
    fn extension_namespace(self) -> &'static str {
        match self {
            Self::Uap3 => UAP3_MANIFEST_NAMESPACE,
            Self::Uap5 => UAP5_MANIFEST_NAMESPACE,
        }
    }

    fn execution_namespace_allowed(self, namespace: Option<&str>) -> bool {
        match self {
            Self::Uap3 => matches!(
                namespace,
                Some(DESKTOP_MANIFEST_NAMESPACE | UAP8_MANIFEST_NAMESPACE)
            ),
            Self::Uap5 => matches!(
                namespace,
                Some(UAP5_MANIFEST_NAMESPACE | UAP8_MANIFEST_NAMESPACE)
            ),
        }
    }
}

pub(crate) fn alias_extension_schema(namespace: Option<&str>) -> Option<AliasExtensionSchema> {
    match namespace {
        Some(UAP3_MANIFEST_NAMESPACE) => Some(AliasExtensionSchema::Uap3),
        Some(UAP5_MANIFEST_NAMESPACE) => Some(AliasExtensionSchema::Uap5),
        _ => None,
    }
}

pub(crate) fn aumid_parts(aumid: &str) -> Option<(&str, &str)> {
    let (family, application_id) = aumid.rsplit_once('!')?;
    (!family.is_empty() && !application_id.is_empty()).then_some((family, application_id))
}

pub(crate) fn windows_ordinal_eq_ignore_case(left: &str, right: &str) -> bool {
    let left = left.encode_utf16().collect::<Vec<_>>();
    let right = right.encode_utf16().collect::<Vec<_>>();
    // SAFETY: both slices are valid UTF-16 buffers for the duration of the
    // synchronous ordinal comparison. No NUL terminator is required.
    (unsafe { CompareStringOrdinal(&left, &right, true) }) == CSTR_EQUAL
}

pub(crate) fn win32_status_error(context: &str, status: u32) -> Box<dyn std::error::Error> {
    format!("{context}: {}", Error::from(HRESULT::from_win32(status))).into()
}

pub(crate) fn normalize_executable_alias(
    value: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let leaf = Path::new(value)
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .filter(|leaf| !leaf.is_empty())
        .ok_or("GUI executable path has no file name")?;
    let mut normalized = leaf.to_ascii_lowercase();
    if Path::new(leaf).extension().is_none() {
        normalized.push_str(".exe");
    }
    Ok(normalized)
}

pub(crate) fn packaged_activation_alias_name(
    value: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let path = Path::new(value);
    if path.is_absolute() || path.components().count() != 1 {
        // A path-qualified executable is not proof that Windows resolved the
        // request through an AppExecutionAlias. Exact-PID capture remains
        // available, but documented packaged delegation is disabled.
        return Ok(None);
    }
    normalize_executable_alias(value).map(Some)
}

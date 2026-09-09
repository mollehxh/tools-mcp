mod credential;

use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::iter::once;
use std::mem::{align_of, size_of, size_of_val, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, CopySid, CreateRestrictedToken, CreateWellKnownSid, DISABLE_MAX_PRIVILEGE, GetLengthSid,
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsTokenRestricted, LUA_TOKEN,
    SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenDefaultDacl,
    TokenGroups, TokenIntegrityLevel, WRITE_RESTRICTED, WinWorldSid,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

const PROTOCOL: &str = "mcp-agent-workspace-write/v1";
const STILL_ACTIVE: u32 = 259;
const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;
const GENERIC_ALL: u32 = 0x1000_0000;

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct AttributeList(LPPROC_THREAD_ATTRIBUTE_LIST);

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.0) };
    }
}

pub fn run(arguments: impl Iterator<Item = OsString>) -> Result<(), String> {
    let arguments = arguments.collect::<Vec<_>>();
    if credential::is_command(arguments.first()) {
        return credential::run(arguments.into_iter());
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "inspect-token")
    {
        return unsafe { inspect_token() };
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "inspect-handle")
    {
        return unsafe { inspect_handle(arguments.get(1)) };
    }
    let launch = Launch::parse(arguments.into_iter())?;
    for root in &launch.write_roots {
        reject_reparse_components(root)?;
    }
    let capability_sid = capability_sid(&launch.write_roots);
    grant_capability(&launch.write_roots, &capability_sid)?;
    let exit_code = unsafe { spawn_restricted(&launch, &capability_sid) }
        .map_err(|error| format!("native launch: {error}"))?;
    std::process::exit(exit_code.cast_signed());
}

unsafe fn inspect_token() -> Result<(), String> {
    let mut token = null_mut();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) == 0 {
        return Err(format!(
            "open current token: {}",
            io::Error::last_os_error()
        ));
    }
    let token = OwnedHandle::new(token).map_err(|error| error.to_string())?;
    let integrity = token_integrity_rid(token.0).map_err(|error| error.to_string())?;
    println!(
        "restricted={} integrity_rid={integrity}",
        IsTokenRestricted(token.0) != 0
    );
    Ok(())
}

unsafe fn inspect_handle(value: Option<&OsString>) -> Result<(), String> {
    let value = value
        .and_then(|value| value.to_str())
        .ok_or("missing handle value")?
        .parse::<usize>()
        .map_err(|_| "invalid handle value")?;
    let mut flags = 0;
    println!(
        "inherited={}",
        GetHandleInformation(value as HANDLE, &raw mut flags) != 0
    );
    Ok(())
}

struct Launch {
    cwd: PathBuf,
    write_roots: Vec<PathBuf>,
    program: OsString,
    args: Vec<OsString>,
}

impl Launch {
    fn parse(mut arguments: impl Iterator<Item = OsString>) -> Result<Self, String> {
        expect_pair(&mut arguments, "--protocol", PROTOCOL)?;
        let workspace = expect_value(&mut arguments, "--workspace")?;
        let cwd = expect_value(&mut arguments, "--cwd")?;
        let mut write_roots = vec![canonical_directory(Path::new(&workspace))?];
        let mut next = arguments.next().ok_or("missing command delimiter")?;
        while next == "--write-root" {
            let root = arguments.next().ok_or("missing write-root value")?;
            write_roots.push(canonical_directory(Path::new(&root))?);
            next = arguments.next().ok_or("missing command delimiter")?;
        }
        if next != "--" {
            return Err("invalid command delimiter".to_owned());
        }
        let program = arguments.next().ok_or("missing child program")?;
        let args = arguments.collect::<Vec<_>>();
        let cwd = canonical_directory(Path::new(&cwd))?;
        if !write_roots.iter().any(|root| cwd.starts_with(root)) {
            return Err("working directory is outside declared roots".to_owned());
        }
        write_roots.sort();
        write_roots.dedup();
        Ok(Self {
            cwd,
            write_roots,
            program,
            args,
        })
    }
}

fn expect_pair(
    arguments: &mut impl Iterator<Item = OsString>,
    name: &str,
    expected: &str,
) -> Result<(), String> {
    if arguments.next().as_deref() != Some(OsStr::new(name))
        || arguments.next().as_deref() != Some(OsStr::new(expected))
    {
        return Err(format!("invalid {name}"));
    }
    Ok(())
}

fn expect_value(
    arguments: &mut impl Iterator<Item = OsString>,
    name: &str,
) -> Result<OsString, String> {
    if arguments.next().as_deref() != Some(OsStr::new(name)) {
        return Err(format!("missing {name}"));
    }
    arguments
        .next()
        .ok_or_else(|| format!("missing {name} value"))
}

fn canonical_directory(path: &Path) -> Result<PathBuf, String> {
    let path = path
        .canonicalize()
        .map_err(|error| format!("canonicalize {}: {error}", path.display()))?;
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()));
    }
    Ok(path)
}

fn reject_reparse_components(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        let wide = wide_null(current.as_os_str());
        let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
        if attributes == u32::MAX {
            return Err(format!(
                "inspect {}: {}",
                current.display(),
                io::Error::last_os_error()
            ));
        }
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!("reparse component denied: {}", current.display()));
        }
    }
    Ok(())
}

fn capability_sid(roots: &[PathBuf]) -> String {
    let mut hasher = Sha256::new();
    for root in roots {
        hasher.update(root.as_os_str().to_string_lossy().to_lowercase().as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let values = digest
        .chunks_exact(4)
        .take(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("four-byte chunk")))
        .collect::<Vec<_>>();
    format!(
        "S-1-5-21-{}-{}-{}-{}",
        values[0], values[1], values[2], values[3]
    )
}

fn grant_capability(roots: &[PathBuf], sid: &str) -> Result<(), String> {
    for root in roots {
        let grant = format!("*{sid}:(OI)(CI)M");
        let status = Command::new("icacls.exe")
            .arg(root)
            .args(["/inheritancelevel:e", "/grant:r"])
            .arg(&grant)
            .status()
            .map_err(|error| format!("start icacls: {error}"))?;
        if !status.success() {
            return Err(format!("icacls rejected capability for {}", root.display()));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
unsafe fn spawn_restricted(launch: &Launch, sid_text: &str) -> io::Result<u32> {
    let mut source_token = null_mut();
    if OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
        &raw mut source_token,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    let source_token = OwnedHandle::new(source_token)?;

    let sid = Sid::from_string(sid_text)?;
    let mut logon_sid = token_logon_sid(source_token.0)?;
    let mut world_sid = well_known_sid(WinWorldSid)?;
    let restrictions = [
        SID_AND_ATTRIBUTES {
            Sid: sid.0,
            Attributes: 0,
        },
        SID_AND_ATTRIBUTES {
            Sid: logon_sid.as_mut_ptr().cast(),
            Attributes: 0,
        },
        SID_AND_ATTRIBUTES {
            Sid: world_sid.as_mut_ptr().cast(),
            Attributes: 0,
        },
    ];
    let mut restricted_token = null_mut();
    if CreateRestrictedToken(
        source_token.0,
        DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
        0,
        null(),
        0,
        null(),
        u32::try_from(restrictions.len()).expect("restriction count fits u32"),
        restrictions.as_ptr(),
        &raw mut restricted_token,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    let restricted_token = OwnedHandle::new(restricted_token)?;
    if IsTokenRestricted(restricted_token.0) == 0 {
        return Err(io::Error::other(
            "CreateRestrictedToken returned an unrestricted token",
        ));
    }
    if token_integrity_rid(restricted_token.0)? > SECURITY_MANDATORY_MEDIUM_RID {
        return Err(io::Error::other(
            "restricted token exceeds medium integrity",
        ));
    }
    set_default_dacl(
        restricted_token.0,
        &[
            sid.0,
            logon_sid.as_mut_ptr().cast(),
            world_sid.as_mut_ptr().cast(),
        ],
    )?;

    let job = OwnedHandle::new(CreateJobObjectW(null(), null()))?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if SetInformationJobObject(
        job.0,
        JobObjectExtendedLimitInformation,
        (&raw const limits).cast::<c_void>(),
        u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap(),
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }

    let std_handles = [
        GetStdHandle(STD_INPUT_HANDLE),
        GetStdHandle(STD_OUTPUT_HANDLE),
        GetStdHandle(STD_ERROR_HANDLE),
    ];
    if std_handles
        .iter()
        .any(|handle| handle.is_null() || *handle == INVALID_HANDLE_VALUE)
    {
        return Err(io::Error::last_os_error());
    }
    let mut inherited_handles = Vec::with_capacity(std_handles.len());
    for handle in std_handles {
        if !inherited_handles.contains(&handle) {
            if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                return Err(io::Error::last_os_error());
            }
            inherited_handles.push(handle);
        }
    }
    let mut attribute_bytes = 0;
    InitializeProcThreadAttributeList(null_mut(), 1, 0, &raw mut attribute_bytes);
    let mut attribute_storage = vec![0_u8; attribute_bytes];
    let attribute_ptr = attribute_storage.as_mut_ptr().cast();
    if InitializeProcThreadAttributeList(attribute_ptr, 1, 0, &raw mut attribute_bytes) == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = AttributeList(attribute_ptr);
    if UpdateProcThreadAttribute(
        attributes.0,
        0,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
        inherited_handles.as_mut_ptr().cast(),
        size_of_val(inherited_handles.as_slice()),
        null_mut(),
        null_mut(),
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut startup: STARTUPINFOEXW = zeroed();
    startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap();
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = std_handles[0];
    startup.StartupInfo.hStdOutput = std_handles[1];
    startup.StartupInfo.hStdError = std_handles[2];
    startup.lpAttributeList = attributes.0;
    let mut process: PROCESS_INFORMATION = zeroed();
    let mut command_line = command_line(&launch.program, &launch.args);
    let cwd = wide_null(launch.cwd.as_os_str());
    if CreateProcessAsUserW(
        restricted_token.0,
        null(),
        command_line.as_mut_ptr(),
        null(),
        null(),
        1,
        CREATE_SUSPENDED
            | CREATE_NEW_PROCESS_GROUP
            | CREATE_UNICODE_ENVIRONMENT
            | EXTENDED_STARTUPINFO_PRESENT,
        null(),
        cwd.as_ptr(),
        &raw const startup.StartupInfo,
        &raw mut process,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    let child_process = OwnedHandle::new(process.hProcess)?;
    let child_thread = OwnedHandle::new(process.hThread)?;
    if AssignProcessToJobObject(job.0, child_process.0) == 0 {
        let error = io::Error::last_os_error();
        TerminateProcess(child_process.0, 125);
        WaitForSingleObject(child_process.0, INFINITE);
        return Err(error);
    }
    if ResumeThread(child_thread.0) == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    if WaitForSingleObject(child_process.0, INFINITE) == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    let mut exit_code = STILL_ACTIVE;
    if GetExitCodeProcess(child_process.0, &raw mut exit_code) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(exit_code)
}

unsafe fn query_token_information(token: HANDLE, class: i32) -> io::Result<Vec<u8>> {
    let mut bytes = 0;
    GetTokenInformation(token, class, null_mut(), 0, &raw mut bytes);
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u8; bytes as usize];
    if GetTokenInformation(
        token,
        class,
        buffer.as_mut_ptr().cast(),
        bytes,
        &raw mut bytes,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(bytes as usize);
    Ok(buffer)
}

unsafe fn token_logon_sid(token: HANDLE) -> io::Result<Vec<u8>> {
    let groups = query_token_information(token, TokenGroups)?;
    if groups.len() < size_of::<u32>() {
        return Err(io::Error::other("token groups are truncated"));
    }
    let count = std::ptr::read_unaligned(groups.as_ptr().cast::<u32>()) as usize;
    let after_count = groups.as_ptr().add(size_of::<u32>()) as usize;
    let alignment = align_of::<SID_AND_ATTRIBUTES>();
    let entries = ((after_count + alignment - 1) & !(alignment - 1)) as *const SID_AND_ATTRIBUTES;
    let entries_offset = entries as usize - groups.as_ptr() as usize;
    let entries_bytes = count
        .checked_mul(size_of::<SID_AND_ATTRIBUTES>())
        .and_then(|bytes| entries_offset.checked_add(bytes))
        .ok_or_else(|| io::Error::other("token group count overflow"))?;
    if entries_bytes > groups.len() {
        return Err(io::Error::other("token groups are truncated"));
    }
    for index in 0..count {
        let entry = std::ptr::read_unaligned(entries.add(index));
        if entry.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID {
            return copy_sid(entry.Sid);
        }
    }
    Err(io::Error::other("current token has no logon SID"))
}

unsafe fn well_known_sid(kind: i32) -> io::Result<Vec<u8>> {
    let mut bytes = 0;
    CreateWellKnownSid(kind, null_mut(), null_mut(), &raw mut bytes);
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut sid = vec![0_u8; bytes as usize];
    if CreateWellKnownSid(kind, null_mut(), sid.as_mut_ptr().cast(), &raw mut bytes) == 0 {
        return Err(io::Error::last_os_error());
    }
    sid.truncate(bytes as usize);
    Ok(sid)
}

unsafe fn copy_sid(sid: *mut c_void) -> io::Result<Vec<u8>> {
    let bytes = GetLengthSid(sid);
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut copy = vec![0_u8; bytes as usize];
    if CopySid(bytes, copy.as_mut_ptr().cast(), sid) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(copy)
}

unsafe fn token_integrity_rid(token: HANDLE) -> io::Result<u32> {
    let label = query_token_information(token, TokenIntegrityLevel)?;
    if label.len() < size_of::<TOKEN_MANDATORY_LABEL>() {
        return Err(io::Error::other("token integrity label is truncated"));
    }
    let label = std::ptr::read_unaligned(label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
    let count = GetSidSubAuthorityCount(label.Label.Sid);
    if count.is_null() || *count == 0 {
        return Err(io::Error::other("token integrity SID is invalid"));
    }
    let rid = GetSidSubAuthority(label.Label.Sid, u32::from(*count) - 1);
    if rid.is_null() {
        return Err(io::Error::other("token integrity RID is unavailable"));
    }
    Ok(*rid)
}

unsafe fn set_default_dacl(token: HANDLE, sids: &[*mut c_void]) -> io::Result<()> {
    let entries = sids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: (*sid).cast(),
            },
        })
        .collect::<Vec<_>>();
    let mut acl: *mut ACL = null_mut();
    let result = SetEntriesInAclW(
        u32::try_from(entries.len()).expect("ACL entry count fits u32"),
        entries.as_ptr(),
        null(),
        &raw mut acl,
    );
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result.cast_signed()));
    }
    let mut default_dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
    let success = SetTokenInformation(
        token,
        TokenDefaultDacl,
        (&raw mut default_dacl).cast(),
        u32::try_from(size_of::<TOKEN_DEFAULT_DACL>()).expect("TOKEN_DEFAULT_DACL size fits u32"),
    );
    LocalFree(acl.cast());
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct Sid(*mut c_void);

impl Sid {
    fn from_string(value: &str) -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        let value = wide_null(OsStr::new(value));
        let mut sid = null_mut();
        if unsafe { ConvertStringSidToSidW(value.as_ptr(), &raw mut sid) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(sid))
        }
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0.cast()) };
    }
}

fn command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut value = quote_windows(program);
    for arg in args {
        value.push(' ');
        value.push_str(&quote_windows(arg));
    }
    wide_null(OsStr::new(&value))
}

fn quote_windows(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if !value.is_empty() && !value.contains([' ', '\t', '"']) {
        return value.into_owned();
    }
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
        } else {
            if character == '"' {
                quoted.push_str(&"\\".repeat(slashes * 2 + 1));
            } else {
                quoted.push_str(&"\\".repeat(slashes));
            }
            slashes = 0;
            quoted.push(character);
        }
    }
    quoted.push_str(&"\\".repeat(slashes * 2));
    quoted.push('"');
    quoted
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(once(0)).collect()
}

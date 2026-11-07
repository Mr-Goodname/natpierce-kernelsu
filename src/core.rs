//! User-requested installation of the original official core into private data.
//! No network request is made by status inspection or daemon startup.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::{fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt}, process::CommandExt}},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub const URL: &str = "https://download.natpierce.cn/linux/natpierce-arm64-v1.06.tar.gz";
pub const SHA256: &str = "7edafed7d1093c27c8ce20458e72ce060243c2204ecdba23aeb6e17ea89cef78";
pub const VERSION: &str = "1.06";
pub const MANIFEST_URL: &str = "https://raw.githubusercontent.com/Mr-Goodname/natpierce-kernelsu/main/core-manifest.json";
const ARCHIVE_LIMIT: u64 = 32 * 1024 * 1024;
const CORE_LIMIT: u64 = 16 * 1024 * 1024;
const MANIFEST_LIMIT: u64 = 64 * 1024;
const RECEIPT_LIMIT: u64 = 16 * 1024;
const RECEIPT_RELEASES: usize = 16;
const STAGE_PREFIX: &str = ".download-";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub installed: bool,
    pub verified: bool,
    pub downloading: bool,
    pub phase: String,
    pub error: String,
    pub version: String,
    pub sha256: String,
    pub url: String,
    pub bytes_downloaded: u64,
    pub installed_version: String,
    pub available_version: String,
    pub update_available: bool,
    pub checking_update: bool,
    pub update_error: String,
}
impl Default for Status {
    fn default() -> Self {
        Self { installed: false, verified: false, downloading: false, phase: "missing".into(), error: String::new(), version: VERSION.into(), sha256: SHA256.into(), url: URL.into(), bytes_downloaded: 0, installed_version: String::new(), available_version: VERSION.into(), update_available: false, checking_update: false, update_error: String::new() }
    }
}

pub fn executable(data: &Path) -> PathBuf { data.join("core/natpierce") }
fn status_path(data: &Path) -> PathBuf { data.join("run/core-download-status.json") }
fn lock_path(data: &Path) -> PathBuf { data.join("run/core-download.lock") }
fn manifest_path(data: &Path) -> PathBuf { data.join("core/manifest-cache.json") }
fn receipt_path(data: &Path) -> PathBuf { data.join("core/installation-receipt.json") }
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Release { version: String, url: String, sha256: String }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest { schema: u32, recommended: String, releases: Vec<Release> }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt { schema: u32, approved: Vec<Release> }

fn builtin_release() -> Release { Release { version: VERSION.into(), url: URL.into(), sha256: SHA256.into() } }
fn version_parts(version: &str) -> io::Result<Vec<u32>> {
    if version.len() > 32 { return Err(invalid("核心版本号过长")); }
    let parts = version.split('.').map(|part| {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) { return Err(invalid("核心版本号应为点分数字")); }
        part.parse::<u32>().map_err(|_| invalid("核心版本号数字超出范围"))
    }).collect::<io::Result<Vec<_>>>()?;
    if !(2..=4).contains(&parts.len()) { return Err(invalid("核心版本号应有 2 至 4 段")); }
    Ok(parts)
}
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left = version_parts(left).unwrap_or_default();
    let mut right = version_parts(right).unwrap_or_default();
    left.resize(4, 0); right.resize(4, 0); left.cmp(&right)
}
fn validate_release(release: &mut Release) -> io::Result<()> {
    version_parts(&release.version)?;
    if release.url != format!("https://download.natpierce.cn/linux/natpierce-arm64-v{}.tar.gz", release.version) {
        return Err(invalid("核心地址必须是对应版本的官方 ARM64 HTTPS 下载地址"));
    }
    if release.sha256.len() != 64 || !release.sha256.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(invalid("核心清单缺少有效 SHA256")); }
    release.sha256.make_ascii_lowercase();
    Ok(())
}
fn validate_manifest(mut manifest: Manifest) -> io::Result<Manifest> {
    if manifest.schema != 1 || manifest.releases.is_empty() || manifest.releases.len() > 128 { return Err(invalid("核心清单 schema 或版本数量无效")); }
    version_parts(&manifest.recommended)?;
    let mut versions = std::collections::HashSet::new();
    for release in &mut manifest.releases {
        validate_release(release)?;
        if !versions.insert(release.version.clone()) { return Err(invalid("核心清单包含重复版本")); }
    }
    if !manifest.releases.iter().any(|r| r.version == manifest.recommended) { return Err(invalid("核心清单没有推荐版本的下载记录")); }
    Ok(manifest)
}
fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit { return Err(invalid("核心元数据必须是体积受限的普通文件")); }
    let mut bytes = Vec::new(); File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { return Err(invalid("核心元数据超过体积上限")); }
    Ok(bytes)
}
fn parse_manifest(bytes: &[u8]) -> io::Result<Manifest> {
    if bytes.len() as u64 > MANIFEST_LIMIT { return Err(invalid("核心清单超过体积上限")); }
    validate_manifest(serde_json::from_slice(bytes).map_err(|_| invalid("核心清单不是有效 JSON"))?)
}
fn cached_manifest(data: &Path) -> Option<Manifest> { read_bounded(&manifest_path(data), MANIFEST_LIMIT).ok().and_then(|b| parse_manifest(&b).ok()) }
fn recommended_release(data: &Path) -> Release {
    cached_manifest(data).and_then(|m| m.releases.into_iter().find(|r| r.version == m.recommended)).unwrap_or_else(builtin_release)
}
fn approved_receipt(data: &Path) -> Vec<Release> {
    let parsed = read_bounded(&receipt_path(data), RECEIPT_LIMIT).ok().and_then(|b| serde_json::from_slice::<Receipt>(&b).ok());
    let Some(mut receipt) = parsed else { return Vec::new(); };
    if receipt.schema != 1 || receipt.approved.is_empty() || receipt.approved.len() > RECEIPT_RELEASES { return Vec::new(); }
    if receipt.approved.iter_mut().any(|r| validate_release(r).is_err()) { return Vec::new(); }
    receipt.approved
}
fn trusted_releases(data: &Path) -> Vec<Release> {
    let mut releases = approved_receipt(data);
    releases.push(builtin_release());
    if let Some(manifest) = cached_manifest(data) { releases.extend(manifest.releases); }
    releases
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 { break; }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_elf(path: &Path) -> io::Result<()> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut header = [0; 64];
    file.read_exact(&mut header).map_err(|_| invalid("官方核心文件过短"))?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 || u16::from_le_bytes([header[18], header[19]]) != 183 {
        return Err(invalid("下载文件不是 ARM64 ELF 核心"));
    }
    let offset = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let size = u16::from_le_bytes(header[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(header[56..58].try_into().unwrap()) as u64;
    if size < 56 || offset > length || count.saturating_mul(size) > length - offset {
        return Err(invalid("官方核心 ELF 头无效"));
    }
    for index in 0..count {
        file.seek(SeekFrom::Start(offset + index * size))?;
        let mut kind = [0; 4];
        file.read_exact(&mut kind)?;
        if u32::from_le_bytes(kind) == 3 { return Err(invalid("官方核心依赖不支持的动态加载器")); }
    }
    Ok(())
}

fn checked_hash(path: &Path) -> io::Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() { return Err(invalid("核心必须是私有目录中的普通文件")); }
    if metadata.len() < 64 || metadata.len() > CORE_LIMIT { return Err(invalid("官方核心文件大小无效")); }
    validate_elf(path)?;
    sha256_file(path)
}
fn verify_with_hash(path: &Path, expected: &str) -> io::Result<()> {
    if checked_hash(path)? != expected { return Err(invalid("官方核心 SHA256 不匹配，拒绝运行或安装")); }
    Ok(())
}
fn installed_release(data: &Path) -> io::Result<Release> {
    for path in [data.to_owned(), data.join("core")] {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() { return Err(invalid("核心必须位于私有普通目录中")); }
    }
    let hash = checked_hash(&executable(data))?;
    trusted_releases(data).into_iter().find(|r| r.sha256 == hash).ok_or_else(|| invalid("官方核心 SHA256 未被可信清单或安装记录认可，拒绝运行"))
}
pub fn verified_path(data: &Path) -> io::Result<PathBuf> {
    installed_release(data)?;
    Ok(executable(data))
}

fn artifact_status(data: &Path) -> Status {
    let mut status = Status::default();
    let available = recommended_release(data);
    status.version = available.version.clone();
    status.available_version = available.version.clone();
    status.sha256 = available.sha256.clone();
    status.url = available.url.clone();
    let path = executable(data);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            status.installed = true;
            match installed_release(data) {
                Ok(installed) => {
                    status.verified = true; status.phase = "ready".into();
                    status.installed_version = installed.version.clone(); status.version = installed.version;
                    status.sha256 = installed.sha256.clone(); status.url = installed.url;
                    status.update_available = compare_versions(&available.version, &status.installed_version) != std::cmp::Ordering::Less && available.sha256 != installed.sha256;
                }
                Err(error) => { status.phase = "error".into(); status.error = format!("官方核心校验失败：{error}"); }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => { status.phase = "error".into(); status.error = format!("无法读取官方核心：{error}"); }
    }
    status
}

fn lock_is_held(data: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(lock_path(data)) else { return false; };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 { return false; }
    let error = io::Error::last_os_error().raw_os_error();
    error == Some(libc::EWOULDBLOCK) || error == Some(libc::EAGAIN)
}

/// Also usable by the CLI when the management daemon is stopped.
pub fn inspect(data: &Path) -> Status {
    let mut status = artifact_status(data);
    if let Ok(bytes) = read_bounded(&status_path(data), MANIFEST_LIMIT) {
        if let Ok(previous) = serde_json::from_slice::<Status>(&bytes) {
            status.update_error = previous.update_error;
            if (previous.downloading || previous.checking_update) && lock_is_held(data) {
                status.downloading = previous.downloading;
                status.checking_update = previous.checking_update;
                status.phase = previous.phase;
                status.bytes_downloaded = previous.bytes_downloaded;
                status.error.clear();
                status.update_error.clear();
            } else if previous.checking_update {
                status.update_error = "上次核心更新检查已中断，请重新检查".into();
            } else if previous.downloading {
                status.phase = "error".into();
                status.error = "上次核心下载已中断，请重新下载".into();
            } else if !previous.error.is_empty() {
                status.phase = "error".into();
                status.error = previous.error;
            }
        }
    }
    status
}

fn private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => return Err(invalid("核心数据目录不能是符号链接或普通文件")),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(error),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn download_lock(data: &Path) -> io::Result<File> {
    private_directory(data)?;
    private_directory(&data.join("run"))?;
    private_directory(&data.join("core"))?;
    let file = OpenOptions::new().create(true).read(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(lock_path(data))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn stage_name(name: &str) -> bool {
    name.strip_prefix(STAGE_PREFIX).is_some_and(|suffix| suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
}
struct Stage(PathBuf);
impl Stage {
    fn create(data: &Path) -> io::Result<Self> {
        let root = data.join("core");
        // Only our uniquely named staging directories are eligible for cleanup.
        // The official identity directory and installed executable are preserved.
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !stage_name(&entry.file_name().to_string_lossy()) { continue; }
            if entry.file_type()?.is_dir() { fs::remove_dir_all(entry.path())?; }
            else { fs::remove_file(entry.path())?; }
        }
        let mut nonce = [0; 16];
        getrandom::getrandom(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
        let name = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let path = root.join(format!("{STAGE_PREFIX}{name}"));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Stage {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

fn program(name: &str) -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from(format!("/system/bin/{name}"))];
    if let Some(paths) = std::env::var_os("PATH") { candidates.extend(std::env::split_paths(&paths).map(|p| p.join(name))); }
    candidates.into_iter().find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}
fn busybox() -> Option<PathBuf> {
    ["/data/adb/ksu/bin/busybox", "/data/adb/ksu/busybox"].iter().map(PathBuf::from).find(|p| p.is_file())
}

fn download_command(destination: &Path, url: &str, limit: u64, seconds: u64) -> io::Result<Command> {
    if let Some(curl) = program("curl") {
        let mut command = Command::new(curl);
        command.args(["--fail", "--location", "--silent", "--show-error", "--proto", "=https", "--proto-redir", "=https", "--connect-timeout", "15", "--max-time"]).arg(seconds.to_string()).args(["--retry", "1", "--max-filesize"]).arg(limit.to_string()).arg("--output").arg(destination).arg(url);
        return Ok(command);
    }
    // The manifest introduces new trusted hashes, so it requires a transport
    // that can forbid redirects to plain HTTP. BusyBox wget cannot guarantee it.
    if url == MANIFEST_URL { return Err(io::Error::other("更新检查需要支持 HTTPS 限制的 curl；内置官方核心仍可下载")); }
    let mut command = if let Some(wget) = program("wget") { Command::new(wget) }
        else if let Some(tool) = busybox() { let mut c = Command::new(tool); c.arg("wget"); c }
        else { return Err(io::Error::other("未找到支持 HTTPS 的 curl 或 KernelSU BusyBox wget")); };
    command.args(["-q", "-T", "15", "-O"]).arg(destination).arg(url);
    Ok(command)
}
fn extract_command(archive: &Path) -> io::Result<Command> {
    let mut command = if let Some(tar) = program("tar") { Command::new(tar) }
        else if let Some(tool) = busybox() { let mut c = Command::new(tool); c.arg("tar"); c }
        else { return Err(io::Error::other("未找到 tar 或 KernelSU BusyBox tar")); };
    // Extract the single member to stdout. No path from the archive is ever
    // used as a filesystem destination, including links or traversal entries.
    command.arg("-xOzf").arg(archive).arg("natpierce");
    Ok(command)
}

fn publish(shared: &Arc<Mutex<Status>>, data: &Path, update: impl FnOnce(&mut Status)) {
    // A refresh must not re-import an old running flag between the in-memory
    // completion update and the persistent status replacement.
    let mut status = shared.lock().unwrap_or_else(|e| e.into_inner());
    update(&mut status);
    let _ = crate::config::save(&status_path(data), &*status);
}

fn command_error(path: &Path) -> String {
    let mut bytes = Vec::new();
    if let Ok(file) = File::open(path) { let _ = file.take(4096).read_to_end(&mut bytes); }
    String::from_utf8_lossy(&bytes).chars().filter(|c| !c.is_control() || *c == '\n').collect::<String>().trim().to_owned()
}

fn run_command(mut command: Command, output: Stdio, stderr: &Path, timeout: Duration, watched: &Path, limit: u64, shared: &Arc<Mutex<Status>>, data: &Path, track_progress: bool) -> io::Result<()> {
    let errors = OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(stderr)?;
    command.stdin(Stdio::null()).stdout(output).stderr(errors);
    let parent = unsafe { libc::getpid() };
    unsafe { command.pre_exec(move || {
        let size = libc::rlimit { rlim_cur: limit as libc::rlim_t, rlim_max: limit as libc::rlim_t };
        if libc::setrlimit(libc::RLIMIT_FSIZE, &size) != 0 { return Err(io::Error::last_os_error()); }
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) != 0 { return Err(io::Error::last_os_error()); }
        if libc::getppid() != parent { libc::_exit(1); }
        Ok(())
    }); }
    let mut child = command.spawn()?;
    let started = Instant::now();
    let mut last_progress = Instant::now();
    loop {
        let length = fs::metadata(watched).map(|m| m.len()).unwrap_or(0);
        if length > limit || started.elapsed() > timeout {
            let _ = child.kill(); let _ = child.wait();
            return Err(io::Error::other(if length > limit { "下载或解压文件超过安全体积上限" } else { "官方核心下载或解压超时，请重试" }));
        }
        if track_progress && last_progress.elapsed() >= Duration::from_secs(1) {
            publish(shared, data, |s| s.bytes_downloaded = length);
            last_progress = Instant::now();
        }
        let exit = match child.try_wait() {
            Ok(exit) => exit,
            Err(error) => { let _ = child.kill(); let _ = child.wait(); return Err(error); }
        };
        if let Some(exit) = exit {
            if exit.success() { return Ok(()); }
            let detail = command_error(stderr);
            return Err(io::Error::other(if detail.is_empty() { format!("外部下载或解压工具退出：{exit}") } else { detail }));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn replace_candidate(data: &Path, candidate: &Path, expected: &str, revision: &AtomicU64) -> io::Result<()> {
    verify_with_hash(candidate, expected)?;
    fs::set_permissions(candidate, fs::Permissions::from_mode(0o700))?;
    File::open(candidate)?.sync_all()?;
    fs::rename(candidate, executable(data))?;
    // Even a directory fsync failure after rename leaves a new executable.
    // Tell the manager to stop the old inode in that case as well.
    revision.fetch_add(1, Ordering::Release);
    File::open(data.join("core"))?.sync_all()
}

fn save_approval(data: &Path, release: &Release) -> io::Result<()> {
    // Persist both sides of the rename before replacing the executable. After
    // a crash the actual digest, rather than a "current" label, chooses which
    // release is installed. Dropping old releases from a new manifest is safe.
    let mut approved = vec![release.clone()];
    if let Ok(current) = installed_release(data) { approved.push(current); }
    approved.extend(approved_receipt(data));
    let mut hashes = std::collections::HashSet::new();
    approved.retain(|r| hashes.insert(r.sha256.clone()));
    approved.truncate(RECEIPT_RELEASES);
    crate::config::save(&receipt_path(data), &Receipt { schema: 1, approved })
}
#[cfg(test)]
fn install_with_hash(data: &Path, candidate: &Path, expected: &str) -> io::Result<()> {
    replace_candidate(data, candidate, expected, &AtomicU64::new(0))
}
fn download_and_install(data: &Path, shared: &Arc<Mutex<Status>>, release: &Release, revision: &AtomicU64) -> io::Result<()> {
    let stage = Stage::create(data)?;
    let archive = stage.0.join("official.tar.gz.part");
    let candidate = stage.0.join("natpierce.part");
    let errors = stage.0.join("command-error.txt");
    let archive_file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&archive)?;
    drop(archive_file);
    publish(shared, data, |s| { s.phase = "downloading".into(); s.error.clear(); });
    run_command(download_command(&archive, &release.url, ARCHIVE_LIMIT, 180)?, Stdio::null(), &errors, Duration::from_secs(200), &archive, ARCHIVE_LIMIT, shared, data, true)?;
    publish(shared, data, |s| { s.phase = "extracting".into(); s.bytes_downloaded = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0); });
    let candidate_file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&candidate)?;
    run_command(extract_command(&archive)?, Stdio::from(candidate_file), &errors, Duration::from_secs(30), &candidate, CORE_LIMIT, shared, data, false)?;
    publish(shared, data, |s| s.phase = "verifying".into());
    verify_with_hash(&candidate, &release.sha256)?;
    save_approval(data, release)?;
    replace_candidate(data, &candidate, &release.sha256, revision)
}
fn check_update(data: &Path, shared: &Arc<Mutex<Status>>) -> io::Result<()> {
    let stage = Stage::create(data)?;
    let destination = stage.0.join("manifest.part");
    let errors = stage.0.join("command-error.txt");
    drop(OpenOptions::new().create_new(true).write(true).mode(0o600).open(&destination)?);
    run_command(download_command(&destination, MANIFEST_URL, MANIFEST_LIMIT, 45)?, Stdio::null(), &errors, Duration::from_secs(60), &destination, MANIFEST_LIMIT, shared, data, false)?;
    let manifest = parse_manifest(&read_bounded(&destination, MANIFEST_LIMIT)?)?;
    commit_manifest(data, &manifest)
}
fn commit_manifest(data: &Path, manifest: &Manifest) -> io::Result<()> {
    // Preserve trust for an executable recognized only by the old cache (for
    // example after a receipt was lost) before the new cache can omit it.
    if let Ok(current) = installed_release(data) { save_approval(data, &current)?; }
    crate::config::save(&manifest_path(data), manifest)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Job { Download, Check, Update }
pub struct Installer {
    data: PathBuf,
    shared: Arc<Mutex<Status>>,
    revision: Arc<AtomicU64>,
}
impl Installer {
    pub fn new(data: PathBuf) -> Self { Self { shared: Arc::new(Mutex::new(inspect(&data))), revision: Arc::new(AtomicU64::new(0)), data } }
    pub fn status(&self) -> Status { self.shared.lock().unwrap_or_else(|e| e.into_inner()).clone() }
    pub fn revision(&self) -> u64 { self.revision.load(Ordering::Acquire) }
    pub fn refresh(&self) {
        // Hold the guard across inspection: otherwise a request may start
        // between the busy check and assignment, losing its running flag.
        let mut current = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if current.downloading || current.checking_update { return; }
        let fresh = inspect(&self.data);
        *current = fresh;
    }
    /// Idempotent and immediate: network, hashing and extraction run in a worker.
    pub fn request_download(&self) -> io::Result<Status> { self.request(Job::Download) }
    pub fn request_check_update(&self) -> io::Result<Status> { self.request(Job::Check) }
    pub fn request_update(&self) -> io::Result<Status> { self.request(Job::Update) }
    fn request(&self, job: Job) -> io::Result<Status> {
        if unsafe { libc::geteuid() } != 0 { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "仅 root 管理服务可以管理官方核心")); }
        let (lock, release, saved) = {
            let mut status = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if status.downloading || status.checking_update || (job == Job::Download && status.verified) { return Ok(status.clone()); }
            let release = recommended_release(&self.data);
            if job == Job::Update && status.verified && (compare_versions(&release.version, &status.installed_version) == std::cmp::Ordering::Less || release.sha256 == status.sha256) { return Ok(status.clone()); }
            // Acquire before publishing/spawning, so no second process can
            // overlap and persisted running flags always have a live flock.
            let lock = download_lock(&self.data)?;
            status.checking_update = job == Job::Check;
            status.downloading = job != Job::Check;
            status.phase = if job == Job::Check { "checking_update" } else { "downloading" }.into();
            if job != Job::Check { status.error.clear(); }
            status.update_error.clear();
            status.bytes_downloaded = 0;
            (lock, release, status.clone())
        };
        if let Err(error) = crate::config::save(&status_path(&self.data), &saved) {
            let fresh = artifact_status(&self.data);
            *self.shared.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
            return Err(error);
        }
        let data = self.data.clone();
        let shared = self.shared.clone();
        let revision = self.revision.clone();
        if let Err(error) = thread::Builder::new().name(if job == Job::Check { "np-core-check" } else { "np-core-download" }.into()).spawn(move || {
            let _lock = lock;
            let previous_revision = revision.load(Ordering::Acquire);
            let result = if job == Job::Check { check_update(&data, &shared) } else { download_and_install(&data, &shared, &release, &revision) };
            let mut fresh = artifact_status(&data);
            let bytes = shared.lock().unwrap_or_else(|e| e.into_inner()).bytes_downloaded;
            fresh.bytes_downloaded = bytes;
            if let Err(error) = result {
                if job == Job::Check { fresh.update_error = format!("核心更新检查失败：{error}"); }
                else {
                    fresh.phase = "error".into();
                    fresh.error = if fresh.verified && revision.load(Ordering::Acquire) != previous_revision {
                        format!("官方核心已安装，但持久化确认失败：{error}")
                    } else { format!("官方核心{}失败：{error}", if job == Job::Update { "升级" } else { "下载" }) };
                    if job == Job::Update { fresh.update_error = fresh.error.clone(); }
                }
            }
            publish(&shared, &data, |status| *status = fresh);
        }) {
            publish(&self.shared, &self.data, |status| {
                status.downloading = false; status.checking_update = false;
                if job == Job::Check { status.update_error = format!("无法启动更新检查任务：{error}"); status.phase = if status.verified { "ready" } else { "missing" }.into(); }
                else { status.phase = "error".into(); status.error = format!("无法启动核心下载任务：{error}"); }
            });
            return Err(error);
        }
        Ok(self.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("np-core-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir_all(path.join("core/data")).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn elf_fixture() -> Vec<u8> {
        let mut bytes = vec![0; 128];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[18..20].copy_from_slice(&183u16.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&1u32.to_le_bytes());
        bytes
    }
    fn release(version: &str, bytes: &[u8]) -> Release {
        Release { version: version.into(), url: format!("https://download.natpierce.cn/linux/natpierce-arm64-v{version}.tar.gz"), sha256: format!("{:x}", Sha256::digest(bytes)) }
    }
    fn manifest(release: Release) -> Manifest { Manifest { schema: 1, recommended: release.version.clone(), releases: vec![release] } }
    #[test]
    fn manifest_rejects_untrusted_urls_missing_hashes_and_ambiguous_versions() {
        let good = release("1.07", &elf_fixture());
        assert!(validate_manifest(manifest(good.clone())).is_ok());
        for url in ["https://download.natpierce.cn.evil.test/linux/natpierce-arm64-v1.07.tar.gz", "http://download.natpierce.cn/linux/natpierce-arm64-v1.07.tar.gz", "https://download.natpierce.cn/linux/natpierce-arm64-v1.07.tar.gz?token=x", "https://download.natpierce.cn/linux/natpierce-arm64-v1.08.tar.gz"] {
            let mut bad = good.clone(); bad.url = url.into(); assert!(validate_manifest(manifest(bad)).is_err());
        }
        let mut bad = good.clone(); bad.sha256.clear(); assert!(validate_manifest(manifest(bad)).is_err());
        let mut duplicate = manifest(good.clone()); duplicate.releases.push(good.clone()); assert!(validate_manifest(duplicate).is_err());
        let mut missing = manifest(good.clone()); missing.recommended = "2.00".into(); assert!(validate_manifest(missing).is_err());
        let mut wrong_schema = manifest(good); wrong_schema.schema = 2; assert!(validate_manifest(wrong_schema).is_err());
        assert!(parse_manifest(&vec![b' '; MANIFEST_LIMIT as usize + 1]).is_err());
        for version in ["1", "1.2.3.4.5", "../1.07", "1.07-beta", "1..07", "1.4294967296"] { assert!(version_parts(version).is_err()); }
        assert!(compare_versions("1.10", "1.09").is_gt());
    }
    #[test]
    fn receipt_recovers_either_side_of_atomic_upgrade_when_manifest_drops_old_version() {
        let fixture = Fixture::new();
        let old_bytes = elf_fixture();
        let old = release("1.07", &old_bytes);
        fs::write(executable(&fixture.0), &old_bytes).unwrap();
        crate::config::save(&manifest_path(&fixture.0), &manifest(old.clone())).unwrap();
        // Record the already trusted old executable, just as its installer did.
        save_approval(&fixture.0, &old).unwrap();
        let mut new_bytes = elf_fixture(); new_bytes[120] = 7;
        let new = release("1.08", &new_bytes);
        crate::config::save(&manifest_path(&fixture.0), &manifest(new.clone())).unwrap();
        save_approval(&fixture.0, &new).unwrap();
        // Crash before rename: the old executable is still recognized.
        assert_eq!(installed_release(&fixture.0).unwrap(), old);
        let candidate = fixture.0.join("core/candidate"); fs::write(&candidate, &new_bytes).unwrap();
        let revision = AtomicU64::new(0);
        replace_candidate(&fixture.0, &candidate, &new.sha256, &revision).unwrap();
        // Crash after rename: the new executable is recognized from the journal.
        fs::remove_file(manifest_path(&fixture.0)).unwrap();
        assert_eq!(installed_release(&fixture.0).unwrap(), new);
        assert_eq!(revision.load(Ordering::Acquire), 1);
        assert_eq!(fs::metadata(receipt_path(&fixture.0)).unwrap().permissions().mode() & 0o777, 0o600);
    }
    #[test]
    fn replacing_manifest_keeps_cache_only_installed_release_trusted() {
        let fixture = Fixture::new(); let bytes = elf_fixture(); let old = release("1.07", &bytes);
        fs::write(executable(&fixture.0), &bytes).unwrap();
        crate::config::save(&manifest_path(&fixture.0), &manifest(old.clone())).unwrap();
        assert!(!receipt_path(&fixture.0).exists());
        let mut new_bytes = bytes; new_bytes[120] = 8;
        let new_manifest = manifest(release("1.08", &new_bytes));
        commit_manifest(&fixture.0, &new_manifest).unwrap();
        assert_eq!(installed_release(&fixture.0).unwrap(), old);
        assert_eq!(cached_manifest(&fixture.0).unwrap().recommended, "1.08");
    }
    #[test]
    fn unknown_executable_requires_an_approved_digest_and_failed_upgrade_keeps_old_core() {
        let fixture = Fixture::new();
        let bytes = elf_fixture(); let old = release("1.07", &bytes);
        fs::write(executable(&fixture.0), &bytes).unwrap();
        assert!(verified_path(&fixture.0).is_err());
        crate::config::save(&manifest_path(&fixture.0), &manifest(old.clone())).unwrap();
        save_approval(&fixture.0, &old).unwrap();
        let identity = fixture.0.join("core/data/config"); fs::write(&identity, b"MAC-PCID").unwrap();
        let candidate = fixture.0.join("core/candidate"); fs::write(&candidate, &bytes).unwrap();
        let revision = AtomicU64::new(0);
        assert!(replace_candidate(&fixture.0, &candidate, SHA256, &revision).is_err());
        assert_eq!(installed_release(&fixture.0).unwrap(), old);
        assert_eq!(fs::read(identity).unwrap(), b"MAC-PCID");
        assert_eq!(revision.load(Ordering::Acquire), 0);
    }
    #[test]
    fn installed_status_preserves_update_check_and_its_error() {
        let fixture = Fixture::new(); let bytes = elf_fixture(); let current = release("1.07", &bytes);
        fs::write(executable(&fixture.0), &bytes).unwrap();
        crate::config::save(&manifest_path(&fixture.0), &manifest(current)).unwrap();
        let lock = download_lock(&fixture.0).unwrap();
        let mut previous = artifact_status(&fixture.0); previous.checking_update = true; previous.phase = "checking_update".into();
        crate::config::save(&status_path(&fixture.0), &previous).unwrap();
        let status = inspect(&fixture.0); assert!(status.verified && status.checking_update); assert_eq!(status.installed_version, "1.07");
        drop(lock);
        let status = inspect(&fixture.0); assert!(status.verified && !status.checking_update); assert!(status.update_error.contains("中断"));
        previous.checking_update = false; previous.update_error = "manifest unreachable".into();
        crate::config::save(&status_path(&fixture.0), &previous).unwrap();
        let status = inspect(&fixture.0); assert!(status.verified); assert_eq!(status.update_error, "manifest unreachable");
    }
    #[test]
    fn same_version_changed_digest_is_upgradeable_but_older_version_is_not() {
        let fixture = Fixture::new(); let bytes = elf_fixture(); let current = release("1.08", &bytes);
        fs::write(executable(&fixture.0), &bytes).unwrap();
        crate::config::save(&manifest_path(&fixture.0), &manifest(current.clone())).unwrap(); save_approval(&fixture.0, &current).unwrap();
        let mut changed = bytes; changed[120] = 42;
        crate::config::save(&manifest_path(&fixture.0), &manifest(release("1.07", &changed))).unwrap(); assert!(!artifact_status(&fixture.0).update_available);
        crate::config::save(&manifest_path(&fixture.0), &manifest(release("1.08", &changed))).unwrap(); assert!(artifact_status(&fixture.0).update_available);
    }
    #[test]
    fn busy_check_is_not_overwritten_by_refresh_or_another_request() {
        let fixture = Fixture::new(); let installer = Installer::new(fixture.0.clone());
        installer.shared.lock().unwrap().checking_update = true;
        installer.refresh(); assert!(installer.status().checking_update);
        // Never launch a network task: every request returns the same busy job.
        if unsafe { libc::geteuid() } == 0 {
            assert!(installer.request_download().unwrap().checking_update);
            assert!(installer.request_update().unwrap().checking_update);
            assert!(installer.request_check_update().unwrap().checking_update);
        }
        assert_eq!(installer.revision(), 0);
    }
    #[test]
    fn invalid_download_preserves_installed_file_and_identity() {
        let fixture = Fixture::new();
        let installed = executable(&fixture.0);
        let config = fixture.0.join("core/data/config");
        fs::write(&installed, b"existing-stored-core").unwrap();
        fs::write(&config, b"preserved-identity").unwrap();
        let candidate = fixture.0.join("core/candidate");
        fs::write(&candidate, elf_fixture()).unwrap();
        assert!(install_with_hash(&fixture.0, &candidate, SHA256).is_err());
        assert_eq!(fs::read(installed).unwrap(), b"existing-stored-core");
        assert_eq!(fs::read(config).unwrap(), b"preserved-identity");
    }
    #[test]
    fn verified_candidate_is_private_and_installed_without_touching_identity() {
        let fixture = Fixture::new();
        let config = fixture.0.join("core/data/config");
        fs::write(&config, b"identity").unwrap();
        let candidate = fixture.0.join("core/candidate");
        let bytes = elf_fixture(); fs::write(&candidate, &bytes).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        install_with_hash(&fixture.0, &candidate, &hash).unwrap();
        assert_eq!(fs::read(executable(&fixture.0)).unwrap(), bytes);
        assert!(!candidate.exists());
        assert_eq!(fs::metadata(executable(&fixture.0)).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read(config).unwrap(), b"identity");
    }
    #[test]
    fn symlink_and_wrong_architecture_cannot_be_installed() {
        let fixture = Fixture::new();
        let candidate = fixture.0.join("core/candidate");
        let outside = fixture.0.join("outside"); let bytes = elf_fixture();
        fs::write(&outside, &bytes).unwrap(); symlink(&outside, &candidate).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        assert!(install_with_hash(&fixture.0, &candidate, &hash).is_err());
        fs::remove_file(&candidate).unwrap();
        let mut wrong = bytes; wrong[18..20].copy_from_slice(&62u16.to_le_bytes());
        fs::write(&candidate, &wrong).unwrap();
        let hash = format!("{:x}", Sha256::digest(&wrong));
        assert!(install_with_hash(&fixture.0, &candidate, &hash).is_err());
        assert!(!executable(&fixture.0).exists());
    }
    #[test]
    fn download_lock_blocks_a_second_job_and_releases_on_drop() {
        let fixture = Fixture::new();
        let first = download_lock(&fixture.0).unwrap();
        assert!(download_lock(&fixture.0).is_err());
        drop(first);
        assert!(download_lock(&fixture.0).is_ok());
    }
    #[test]
    fn missing_core_and_interrupted_download_are_distinct() {
        let fixture = Fixture::new();
        assert_eq!(inspect(&fixture.0).phase, "missing");
        fs::create_dir_all(fixture.0.join("run")).unwrap();
        let mut interrupted = Status::default(); interrupted.downloading = true; interrupted.phase = "downloading".into();
        crate::config::save(&status_path(&fixture.0), &interrupted).unwrap();
        let status = inspect(&fixture.0);
        assert_eq!(status.phase, "error"); assert!(!status.downloading); assert!(status.error.contains("中断"));
    }
}

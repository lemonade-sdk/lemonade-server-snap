//! A minimal `podman` stand-in for the strictly confined lemonade-server snap.
//!
//! Strict confinement cannot run a container runtime (no mount, unshare or
//! pivot_root), but lemond's container backends only need an OCI image's files
//! and a process started from them. This implements just the podman subset
//! lemond uses (info, pull, image inspect, rmi, network create/rm, run, stop,
//! rm, inspect): images are pulled from the registry and unpacked under
//! $SNAP_COMMON, and `run` starts the image's program through the image's own
//! dynamic loader, so the snap's confinement is the sandbox.

use flate2::read::GzDecoder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{exit, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

const ALLOWED: [&str; 2] = ["docker.io/kyuz0/", "ghcr.io/peonist-ai/"];
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json,\
application/vnd.docker.distribution.manifest.list.v2+json,\
application/vnd.oci.image.manifest.v1+json,\
application/vnd.docker.distribution.manifest.v2+json";

type Res<T> = Result<T, String>;

fn die<T>(msg: impl AsRef<str>) -> T {
    eprintln!("Error: {}", msg.as_ref());
    exit(125)
}

fn root() -> PathBuf {
    let base = std::env::var("SNAP_COMMON").unwrap_or_else(|_| "/var/tmp".into());
    Path::new(&base).join("oci")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn image_dir(reference: &str) -> PathBuf {
    let h = hex(&Sha256::digest(reference.as_bytes()));
    root().join("images").join(&h[..24])
}

fn container_file(name: &str) -> PathBuf {
    root().join("containers").join(format!("{name}.json"))
}

fn machine_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn chmod(path: &Path, mode: u32) {
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

// ---------------------------------------------------------------- registry

struct Reference {
    registry: String,
    repo: String,
    digest: String,
}

fn split_ref(reference: &str) -> Res<Reference> {
    if !ALLOWED.iter().any(|p| reference.starts_with(p)) {
        return Err(format!("image {reference} is not from an allowed registry namespace"));
    }
    let (registry, rest) = reference.split_once('/').unwrap();
    let (repo, digest) = rest
        .split_once('@')
        .ok_or_else(|| format!("image {reference} is not pinned by digest"))?;
    Ok(Reference { registry: registry.into(), repo: repo.into(), digest: digest.into() })
}

struct Registry {
    agent: ureq::Agent,
    host: String,
    repo: String,
    token: Option<String>,
}

impl Registry {
    fn new(r: &Reference) -> Self {
        let host = if r.registry == "docker.io" { "registry-1.docker.io" } else { &r.registry };
        Registry {
            agent: ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(30)).build(),
            host: host.into(),
            repo: r.repo.clone(),
            token: None,
        }
    }

    fn get(&mut self, path: &str, accept: &str) -> Res<ureq::Response> {
        let url = format!("https://{}/v2/{}/{}", self.host, self.repo, path);
        for attempt in 0..2 {
            let mut req = self.agent.get(&url).set("Accept", accept);
            if let Some(t) = &self.token {
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
            match req.call() {
                Ok(resp) => return Ok(resp),
                Err(ureq::Error::Status(401, resp)) if attempt == 0 => {
                    let challenge = resp.header("WWW-Authenticate").unwrap_or("").to_string();
                    self.authenticate(&challenge)?;
                }
                Err(e) => return Err(format!("registry request {url} failed: {e}")),
            }
        }
        Err("registry authentication failed".into())
    }

    fn authenticate(&mut self, challenge: &str) -> Res<()> {
        let param = |key: &str| -> Option<String> {
            let needle = format!("{key}=\"");
            let start = challenge.find(&needle)? + needle.len();
            let end = challenge[start..].find('"')? + start;
            Some(challenge[start..end].to_string())
        };
        let realm = param("realm").ok_or("registry requires unsupported authentication")?;
        let service = param("service").unwrap_or_default();
        let resp = self
            .agent
            .get(&realm)
            .query("service", &service)
            .query("scope", &format!("repository:{}:pull", self.repo))
            .call()
            .map_err(|e| format!("token request failed: {e}"))?;
        let mut body = Vec::new();
        resp.into_reader().read_to_end(&mut body).map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        self.token = v["token"].as_str().or(v["access_token"].as_str()).map(String::from);
        Ok(())
    }

    fn manifest(&mut self, digest: &str) -> Res<Value> {
        let mut body = Vec::new();
        self.get(&format!("manifests/{digest}"), MANIFEST_TYPES)?
            .into_reader()
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
        if let Some(want) = digest.strip_prefix("sha256:") {
            if hex(&Sha256::digest(&body)) != want {
                return Err(format!("manifest digest mismatch for {digest}"));
            }
        }
        serde_json::from_slice(&body).map_err(|e| e.to_string())
    }
}

/// Reader that hashes everything read through it.
struct HashReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> Read for HashReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// A path inside the image with no way to climb out of it, or None to skip.
fn clean_rel(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir | Component::RootDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() { None } else { Some(out) }
}

fn remove_any(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

fn unpack_layer<R: Read>(reader: R, dest: &Path) -> Res<()> {
    let real_dest = fs::canonicalize(dest).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let Some(rel) = clean_rel(&path) else { continue };
        let name = rel.file_name().unwrap().to_string_lossy().into_owned();
        let parent_rel = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        let parent = dest.join(&parent_rel);

        if name == ".wh..wh..opq" {
            if let Ok(rd) = fs::read_dir(&parent) {
                rd.flatten().for_each(|e| remove_any(&e.path()));
            }
            continue;
        }
        if let Some(victim) = name.strip_prefix(".wh.") {
            remove_any(&parent.join(victim));
            continue;
        }

        let kind = entry.header().entry_type();
        if !(kind.is_dir() || kind.is_file() || kind.is_symlink() || kind.is_hard_link()) {
            continue;
        }
        fs::create_dir_all(&parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        // A symlink earlier in the layer must not steer writes out of the image.
        match fs::canonicalize(&parent) {
            Ok(p) if p.starts_with(&real_dest) => {}
            _ => {
                eprintln!("skipping {}: path leaves the image", rel.display());
                continue;
            }
        }
        let target = parent.join(&name);
        let mode = entry.header().mode().unwrap_or(0o644) & 0o777;

        if kind.is_dir() {
            if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
                remove_any(&target);
                fs::create_dir(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            }
            // Directories stay writable so later layers and removal work.
            chmod(&target, mode | 0o700);
            continue;
        }
        remove_any(&target);
        if kind.is_symlink() {
            let link = entry.link_name().map_err(|e| e.to_string())?.ok_or("symlink without target")?;
            symlink(link, &target).map_err(|e| format!("{}: {e}", target.display()))?;
        } else if kind.is_hard_link() {
            let link = entry.link_name().map_err(|e| e.to_string())?.ok_or("link without target")?;
            let Some(src_rel) = clean_rel(&link) else { continue };
            let src = dest.join(src_rel);
            if fs::hard_link(&src, &target).is_err() {
                fs::copy(&src, &target).map_err(|e| format!("{}: {e}", target.display()))?;
            }
        } else {
            let mut f = fs::File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            io::copy(&mut entry, &mut f).map_err(|e| format!("{}: {e}", target.display()))?;
            chmod(&target, mode | 0o600);
        }
    }
    Ok(())
}

/// ICD manifests name their driver by an absolute path inside the image.
fn rewrite_vulkan_manifests(rootfs: &Path) {
    for sub in ["usr/share/vulkan/icd.d", "etc/vulkan/icd.d"] {
        let Ok(rd) = fs::read_dir(rootfs.join(sub)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(text) = fs::read_to_string(&p) else { continue };
            let Ok(mut v) = serde_json::from_str::<Value>(&text) else { continue };
            let Some(lib) = v["ICD"]["library_path"].as_str().map(String::from) else { continue };
            if lib.starts_with('/') {
                v["ICD"]["library_path"] = json!(format!("{}{}", rootfs.display(), lib));
                let _ = fs::write(&p, v.to_string());
            }
        }
    }
}

fn cmd_pull(args: &[String]) -> Res<()> {
    let refs: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if refs.len() != 1 {
        return Err("pull takes one image".into());
    }
    let reference = refs[0];
    let parsed = split_ref(reference)?;
    let mut reg = Registry::new(&parsed);
    let mut manifest = reg.manifest(&parsed.digest)?;
    if let Some(list) = manifest["manifests"].as_array() {
        let pick = list.iter().find(|m| {
            m["platform"]["os"] == "linux" && m["platform"]["architecture"] == machine_arch()
        });
        let digest = pick
            .and_then(|m| m["digest"].as_str())
            .ok_or_else(|| format!("no linux/{} image in {reference}", machine_arch()))?
            .to_string();
        manifest = reg.manifest(&digest)?;
    }

    let dest = image_dir(reference);
    let staging = PathBuf::from(format!("{}.partial", dest.display()));
    remove_any(&staging);
    let rootfs = staging.join("rootfs");
    fs::create_dir_all(&rootfs).map_err(|e| e.to_string())?;

    for layer in manifest["layers"].as_array().ok_or("manifest has no layers")? {
        let digest = layer["digest"].as_str().ok_or("layer without digest")?;
        let media = layer["mediaType"].as_str().unwrap_or("");
        if media.contains("zstd") {
            return Err("zstd-compressed layers are not supported".into());
        }
        println!("Copying blob {digest}");
        let resp = reg.get(&format!("blobs/{digest}"), "")?;
        let mut hashed = HashReader { inner: resp.into_reader(), hasher: Sha256::new() };
        if media.ends_with("gzip") {
            unpack_layer(GzDecoder::new(&mut hashed), &rootfs)?;
        } else {
            unpack_layer(&mut hashed, &rootfs)?;
        }
        io::copy(&mut hashed, &mut io::sink()).map_err(|e| e.to_string())?;
        if Some(hex(&hashed.hasher.finalize())) != digest.strip_prefix("sha256:").map(String::from) {
            return Err(format!("blob digest mismatch for {digest}"));
        }
        println!("Copying blob {digest} done");
    }
    rewrite_vulkan_manifests(&rootfs);
    fs::write(staging.join("image.json"), json!({"ref": reference, "id": parsed.digest}).to_string())
        .map_err(|e| e.to_string())?;
    remove_any(&dest);
    fs::rename(&staging, &dest).map_err(|e| e.to_string())?;
    println!("{}", parsed.digest);
    Ok(())
}

fn cmd_image(args: &[String]) -> Res<()> {
    if args.first().map(String::as_str) == Some("inspect") {
        let reference = args.last().ok_or("image inspect needs an image")?;
        let meta = image_dir(reference).join("image.json");
        let text = fs::read_to_string(&meta).map_err(|_| format!("no such image: {reference}"))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        println!("{}", v["id"].as_str().unwrap_or(""));
        return Ok(());
    }
    Err("unsupported image command".into())
}

fn make_writable(path: &Path) {
    chmod(path, 0o700);
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                make_writable(&e.path());
            }
        }
    }
}

fn cmd_rmi(args: &[String]) -> Res<()> {
    let reference = args.last().ok_or("rmi needs an image")?;
    let dir = image_dir(reference);
    if !dir.is_dir() {
        return Err(format!("no such image: {reference}"));
    }
    make_writable(&dir);
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

// --------------------------------------------------------------- run / stop

static CHILD: AtomicI32 = AtomicI32::new(0);
static PENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    PENDING.store(sig, Ordering::SeqCst);
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::killpg(pid, sig) };
    }
}

fn first_existing(base: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(|c| base.join(c)).find(|p| p.exists())
}

fn cmd_run(args: &[String]) -> Res<()> {
    let mut name = None;
    let mut publish = None;
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut mounts: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    let mut image = None;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--name" | "-v" | "--env" | "-p" | "--label" | "--network" | "--device"
            | "--group-add" | "--annotation" | "--security-opt" => {
                let v = args.get(i + 1).ok_or(format!("{a} needs a value"))?.clone();
                i += 2;
                match a {
                    "--name" => name = Some(v),
                    "-p" => publish = Some(v),
                    "--env" => {
                        let (k, val) = v.split_once('=').unwrap_or((&v, ""));
                        env.insert(k.into(), val.into());
                    }
                    "-v" => {
                        let mut parts = v.split(':');
                        if let (Some(src), Some(dst)) = (parts.next(), parts.next()) {
                            mounts.push((src.into(), dst.into()));
                        }
                    }
                    _ => {}
                }
            }
            _ if a.starts_with('-') => i += 1,
            _ => {
                image = Some(a.to_string());
                i += 1;
                break;
            }
        }
    }
    let mut command: Vec<String> = args[i..].to_vec();
    let (Some(name), Some(image)) = (name, image) else {
        return Err("run needs --name, an image and a command".into());
    };
    if command.is_empty() {
        return Err("run needs a command".into());
    }

    let dir = image_dir(&image);
    let rootfs = dir.join("rootfs");
    if !dir.join("image.json").exists() {
        return Err(format!("image {image} is not installed"));
    }

    // Stand in for the /mnt/models bind mounts with a directory of symlinks.
    let run_dir = root().join("run").join(&name);
    remove_any(&run_dir);
    let models = run_dir.join("models");
    let home = run_dir.join("home");
    fs::create_dir_all(&models).and_then(|_| fs::create_dir_all(&home)).map_err(|e| e.to_string())?;
    for (src, dst) in &mounts {
        let rel = Path::new(dst).strip_prefix("/mnt/models").unwrap_or(Path::new(dst));
        let link = models.join(rel);
        if let Some(p) = link.parent() {
            fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        symlink(src, &link).map_err(|e| e.to_string())?;
    }
    for a in command.iter_mut() {
        *a = a.replace("/mnt/models", &models.to_string_lossy());
    }
    // The container publishes its port on loopback only; with no network
    // namespace here, 0.0.0.0 would expose the server on every interface.
    if publish.as_deref().map_or(false, |p| p.starts_with("127.0.0.1:")) {
        for j in 0..command.len().saturating_sub(1) {
            if command[j] == "--host" && command[j + 1] == "0.0.0.0" {
                command[j + 1] = "127.0.0.1".into();
            }
        }
    }

    let libdirs: Vec<String> = ["usr/local/lib64", "usr/local/lib", "usr/lib64", "usr/lib", "lib64", "lib"]
        .iter()
        .map(|d| rootfs.join(d).to_string_lossy().into_owned())
        .collect();
    let loader = first_existing(
        &rootfs,
        &[
            "usr/lib64/ld-linux-x86-64.so.2",
            "lib64/ld-linux-x86-64.so.2",
            "usr/lib/ld-linux-aarch64.so.1",
            "usr/lib64/ld-linux-aarch64.so.1",
        ],
    )
    .ok_or("image has no dynamic loader")?;
    let program = if command[0].starts_with('/') {
        PathBuf::from(format!("{}{}", rootfs.display(), command[0]))
    } else {
        first_existing(&rootfs, &[&format!("usr/local/bin/{0}", command[0]), &format!("usr/bin/{0}", command[0]), &format!("bin/{0}", command[0])])
            .ok_or_else(|| format!("{} not found in image", command[0]))?
    };

    let mut icds: Vec<String> = fs::read_dir(rootfs.join("usr/share/vulkan/icd.d"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |x| x == "json"))
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    icds.sort();

    let mut cmd = Command::new(&loader);
    cmd.env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("VK_DRIVER_FILES", icds.join(":"));
    for (k, v) in &env {
        if k != "HOME" {
            cmd.env(k, v);
        }
    }
    cmd.arg("--library-path").arg(libdirs.join(":")).arg(&program).args(&command[1..]);
    println!("lemonade oci shim: {:?}", cmd);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            // Die with the shim, as a container dies with its `podman run` client.
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }

    for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(s, forward as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", program.display()))?;
    let pid = child.id() as i32;
    CHILD.store(pid, Ordering::SeqCst);
    if PENDING.load(Ordering::SeqCst) != 0 {
        unsafe { libc::killpg(pid, PENDING.load(Ordering::SeqCst)) };
    }
    let state = container_file(&name);
    fs::create_dir_all(state.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(&state, json!({"name": name, "pid": pid}).to_string()).map_err(|e| e.to_string())?;

    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&state);
    remove_any(&run_dir);
    use std::os::unix::process::ExitStatusExt;
    exit(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)));
}

fn read_pid(name: &str) -> Option<i32> {
    let v: Value = serde_json::from_str(&fs::read_to_string(container_file(name)).ok()?).ok()?;
    v["pid"].as_i64().map(|p| p as i32)
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn cmd_stop(args: &[String]) -> Res<()> {
    let mut timeout = 10u64;
    let mut names = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--time" || args[i] == "-t" {
            timeout = args.get(i + 1).and_then(|t| t.parse().ok()).unwrap_or(10);
            i += 2;
        } else {
            names.push(args[i].clone());
            i += 1;
        }
    }
    for name in names {
        let pid = read_pid(&name).ok_or_else(|| format!("no such container: {name}"))?;
        unsafe { libc::killpg(pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(timeout);
        while Instant::now() < deadline && alive(pid) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if alive(pid) {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
        println!("{name}");
    }
    Ok(())
}

fn cmd_rm(args: &[String]) -> Res<()> {
    for name in args.iter().filter(|a| !a.starts_with('-')) {
        if let Some(pid) = read_pid(name) {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            let _ = fs::remove_file(container_file(name));
            remove_any(&root().join("run").join(name));
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else { die("no command") };
    let _ = fs::create_dir_all(root());
    let result = match cmd.as_str() {
        "info" => {
            // lemond reads: Rootless NetworkBackend OCIRuntime.
            if rest.iter().any(|a| a == "--format") {
                println!("true netavark crun");
            } else {
                println!("host:\n  security:\n    rootless: true\n  networkBackend: netavark\n  ociRuntime:\n    name: crun");
            }
            Ok(())
        }
        "pull" => cmd_pull(rest),
        "image" => cmd_image(rest),
        "rmi" => cmd_rmi(rest),
        // Containers share the snap's network namespace.
        "network" => Ok(()),
        "run" => cmd_run(rest),
        "stop" => cmd_stop(rest),
        "rm" => cmd_rm(rest),
        "inspect" => {
            println!();
            Ok(())
        }
        "--version" | "version" => {
            println!("podman version 5.0.0-lemonade-snap-shim");
            Ok(())
        }
        other => Err(format!("unsupported command: {other}")),
    };
    if let Err(e) = result {
        let code = if e.starts_with("no such image") { 1 } else { 125 };
        eprintln!("Error: {e}");
        exit(code);
    }
}

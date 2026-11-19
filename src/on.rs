//! `wits on <where> ...`: ask wits on another machine and bring the answer
//! back.
//!
//! All wits needs of the other machine is a command that runs a shell
//! command line there, with its input and output connected: `ssh`, or a
//! cloud's own tool for it (`gcloud compute ssh`, `az ssh vm`, ssh through
//! AWS Session Manager), which also find the machine by its name and log in
//! with the user's cloud account. Over that, wits runs itself: the binary is
//! put in `~/.cache/wits` there the first time, and the arguments are passed
//! as they are. Files the answer is written to (`--html`, `-o`) are written
//! there and brought back.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASES: &str = "https://github.com/aryehlev/wits/releases";
/// What the script on the other machine exits with when wits is not there.
const ABSENT: i32 = 197;

const USAGE: &str = "\
wits on <where> [wits arguments...]
    ask wits on another machine: everything after <where> is what you would
    type there (a pid or name, profile, run -- <command>, --json, --html ...)

WHERE
  [user@]host[:port]         over ssh, with your keys and ~/.ssh/config: any
                             machine you can ssh to, in any cloud or none
  gcp:[user@][project/][zone/]<instance>
                             a Google Cloud VM, through gcloud (which tunnels
                             through IAP when the VM has no public address)
  aws:[user@][region/]<instance id or Name tag>
                             an EC2 instance, by ssh through Session Manager:
                             no open port or public address needed
  azure:[user@]<resource group>/<vm>
                             an Azure VM, through az ssh
  --via '<command>'          instead of <where>: any command that runs the
                             shell command line given as its last argument
                             on the machine, such as
                               --via 'ssh -J bastion -p 2222 web-1'
                               --via 'docker exec -i web sh -c'
                               --via 'tsh ssh web-1'

OPTIONS (before <where> or --via)
  --sudo   run wits there as root (sudo -n): it then sees other users'
           processes and, on Linux, where on-CPU time goes
  --rm     remove wits from the machine afterwards

The first time, wits puts its binary for that machine (a few MB) in
~/.cache/wits there. It changes nothing else, and nothing has to be
installed or listening there. For Kubernetes pods use kubectl wits.
";

/// How to run a shell command line on the other machine: these words, then
/// the command line as one more word.
#[derive(Debug)]
pub struct Via {
    argv: Vec<String>,
    /// Goes before the command line in its word (`--command=`).
    last: &'static str,
    /// Words after the command line.
    after: Vec<String>,
    /// What to call the machine.
    name: String,
    /// What has to be installed here for it, said when the program is missing.
    needs: &'static str,
}

/// Made of characters that neither a shell nor an option parser reads.
fn plain(s: &str, also: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c) || also.contains(c))
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// The way to a machine, from its name. `batch`: nobody is there to answer
/// a question (a password, a new host key), so ssh must fail instead of
/// asking on a terminal this process only seems to have.
pub fn place(w: &str, batch: bool) -> Result<Via, String> {
    let quiet: &[&str] = if batch { &["-o", "BatchMode=yes"] } else { &[] };
    let not = |what: &str| format!("'{w}' is not {what} (try `wits on --help`)");
    let (scheme, rest) = match w.split_once(':') {
        Some((s, r)) if matches!(s, "gcp" | "gce" | "gcloud" | "aws" | "ec2" | "azure" | "az") => (s, r),
        _ => ("ssh", w.strip_prefix("ssh://").unwrap_or(w)),
    };
    let (user, rest) = match rest.rsplit_once('@') {
        Some((u, r)) => (Some(u), r),
        None => (None, rest),
    };
    if user.is_some_and(|u| !plain(u, "")) {
        return Err(not("a user and a machine"));
    }
    let dest = |host: &str| match user {
        Some(u) => format!("{u}@{host}"),
        None => host.to_string(),
    };
    match scheme {
        "gcp" | "gce" | "gcloud" => {
            let parts: Vec<&str> = rest.split('/').collect();
            let (project, zone, inst) = match parts.as_slice() {
                [i] => (None, None, *i),
                [z, i] => (None, Some(*z), *i),
                [p, z, i] => (Some(*p), Some(*z), *i),
                _ => return Err(not("gcp:[user@][project/][zone/]instance")),
            };
            if !plain(inst, "") || zone.is_some_and(|z| !plain(z, "")) || project.is_some_and(|p| !plain(p, ":")) {
                return Err(not("gcp:[user@][project/][zone/]instance"));
            }
            let mut argv = strs(&["gcloud", "compute", "ssh", &dest(inst)]);
            if let Some(z) = zone {
                argv.extend(strs(&["--zone", z]));
            }
            if let Some(p) = project {
                argv.extend(strs(&["--project", p]));
            }
            Ok(Via {
                argv,
                last: "--command=",
                after: if batch { strs(&["--", "-o", "BatchMode=yes"]) } else { Vec::new() },
                name: inst.to_string(),
                needs: "the Google Cloud CLI (gcloud), logged in",
            })
        }
        "aws" | "ec2" => {
            let (region, inst) = match rest.split_once('/') {
                Some((r, i)) => (Some(r), i),
                None => (None, rest),
            };
            if !plain(inst, "") || region.is_some_and(|r| !plain(r, "")) {
                return Err(not("aws:[user@][region/]<instance id or Name tag>"));
            }
            let id = match inst.strip_prefix("i-").or(inst.strip_prefix("mi-")) {
                Some(hex) if hex.len() >= 8 && hex.chars().all(|c| c.is_ascii_hexdigit()) => inst.to_string(),
                _ => ec2_by_name(inst, region)?,
            };
            let region = region.map(|r| format!(" --region {r}")).unwrap_or_default();
            // As the Session Manager guide gives it for ~/.ssh/config.
            let proxy = format!(
                "ProxyCommand=sh -c \"aws ssm start-session{region} --target %h --document-name AWS-StartSSHSession --parameters 'portNumber=%p'\""
            );
            let mut argv = strs(&["ssh"]);
            argv.extend(strs(quiet));
            argv.extend(strs(&["-o", &proxy, "--", &dest(&id)]));
            Ok(Via {
                argv,
                last: "",
                after: Vec::new(),
                name: inst.to_string(),
                needs: "ssh, and for the way in the AWS CLI with its Session Manager plugin",
            })
        }
        "azure" | "az" => {
            let Some((group, vm)) = rest.split_once('/') else {
                return Err(not("azure:[user@]<resource group>/<vm>"));
            };
            if !plain(group, "()") || !plain(vm, "") {
                return Err(not("azure:[user@]<resource group>/<vm>"));
            }
            let mut argv = strs(&["az", "ssh", "vm", "--resource-group", group, "--name", vm]);
            if let Some(u) = user {
                argv.extend(strs(&["--local-user", u]));
            }
            argv.push("--".into());
            argv.extend(strs(quiet));
            Ok(Via {
                argv,
                last: "",
                after: Vec::new(),
                name: vm.to_string(),
                needs: "the Azure CLI (az) with its ssh extension, logged in",
            })
        }
        _ => {
            if rest.contains('/') {
                return Err(format!(
                    "'{w}' is not a machine wits can reach (for a Kubernetes pod use kubectl wits; try `wits on --help`)"
                ));
            }
            // host, host:port, [v6]:port or a bare IPv6 address.
            let (host, port) = match rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
                Some((h, p)) => (h, p.strip_prefix(':')),
                None => match rest.split_once(':') {
                    Some((h, p)) if !p.contains(':') => (h, Some(p)),
                    _ => (rest, None),
                },
            };
            if !plain(host, ":%") || port.is_some_and(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
                return Err(not("[user@]host[:port]"));
            }
            let mut argv = strs(&["ssh"]);
            argv.extend(strs(quiet));
            if let Some(p) = port {
                argv.extend(strs(&["-p", p]));
            }
            argv.extend(strs(&["--", &dest(host)]));
            Ok(Via {
                argv,
                last: "",
                after: Vec::new(),
                name: host.to_string(),
                needs: "ssh",
            })
        }
    }
}

/// The id of the one running EC2 instance with this Name tag.
fn ec2_by_name(name: &str, region: Option<&str>) -> Result<String, String> {
    let mut c = Command::new("aws");
    c.args(["ec2", "describe-instances"]);
    if let Some(r) = region {
        c.args(["--region", r]);
    }
    c.args(["--filters", &format!("Name=tag:Name,Values={name}"), "Name=instance-state-name,Values=running"]);
    c.args(["--query", "Reservations[].Instances[].InstanceId", "--output", "text"]);
    let out = c
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run aws to find the instance called '{name}': {e} (give its id instead: aws:i-...)"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ids: Vec<&str> = text.split_whitespace().collect();
    match ids.as_slice() {
        [id] if out.status.success() && plain(id, "") => Ok(id.to_string()),
        [] => Err(format!("no running EC2 instance has the Name tag '{name}' (give its id instead: aws:i-...)")),
        _ if !out.status.success() => Err(format!("aws could not look up the instance called '{name}'")),
        _ => Err(format!("{} running instances have the Name tag '{name}': {}; give one id", ids.len(), ids.join(", "))),
    }
}

/// Words as a shell would split them: spaces, and '...' or "..." around
/// what has spaces in it.
fn words(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut quote = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.get_or_insert_with(String::new).push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                cur.get_or_insert_with(String::new);
            }
            None if c.is_whitespace() => out.extend(cur.take()),
            None => cur.get_or_insert_with(String::new).push(c),
        }
    }
    if quote.is_some() {
        return Err(format!("--via: a quote is not closed in: {s}"));
    }
    out.extend(cur);
    Ok(out)
}

fn via(cmd: &str) -> Result<Via, String> {
    let argv = words(cmd)?;
    if argv.is_empty() {
        return Err("--via needs a command that runs a shell command line on the machine".into());
    }
    Ok(Via {
        name: "that machine".into(),
        argv,
        last: "",
        after: Vec::new(),
        needs: "",
    })
}

/// One word of a shell command line.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Via {
    /// The command that runs `script` there. A POSIX shell runs it, whatever
    /// the user's shell on that machine is.
    fn command(&self, script: &str) -> Command {
        let mut c = Command::new(&self.argv[0]);
        c.args(&self.argv[1..]);
        c.arg(format!("{}sh -c {}", self.last, quote(script)));
        c.args(&self.after);
        c
    }

    /// The way there, to show when it fails.
    fn shown(&self) -> String {
        let mut s: Vec<String> =
            self.argv.iter().map(|a| if a.contains([' ', '"', '\'']) { quote(a) } else { a.clone() }).collect();
        s.push(format!("{}'sh -c ...'", self.last));
        s.extend(self.after.iter().cloned());
        s.join(" ")
    }

    fn cannot(&self, e: &std::io::Error) -> String {
        if e.kind() == std::io::ErrorKind::NotFound && self.needs.is_empty() {
            format!("`{}` is not a command here", self.argv[0])
        } else if e.kind() == std::io::ErrorKind::NotFound {
            format!("`{}` is not installed here. Reaching {} takes {}", self.argv[0], self.name, self.needs)
        } else {
            format!("cannot run `{}`: {e}", self.argv[0])
        }
    }

    /// Run `script` there and return what it printed. `input` is its
    /// standard input.
    fn read(&self, script: &str, input: Option<File>) -> Result<(i32, Vec<u8>), String> {
        let out = self
            .command(script)
            .stdin(input.map(Stdio::from).unwrap_or_else(Stdio::null))
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| self.cannot(&e))?;
        Ok((out.status.code().unwrap_or(1), out.stdout))
    }
}

/// Where wits lives on the other machine: `$d` the directory, `$w` the binary.
fn there() -> String {
    format!("d=\"${{XDG_CACHE_HOME:-$HOME/.cache}}/wits\"; w=\"$d/wits-{VERSION}\"")
}

/// An argument for the wits over there.
#[derive(Debug, PartialEq)]
enum Word {
    Same(String),
    /// A file wits writes there, by its name in `$d`, to bring back.
    File(String),
}

/// The arguments as the other machine gets them, and the files to bring
/// back: (the path here, the name there).
fn rewrite(args: &[String], tag: &str) -> (Vec<Word>, Vec<(String, String)>) {
    let (mut out, mut files) = (Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        out.push(Word::Same(a.clone()));
        if a == "--" {
            // The command of `wits run`: not ours to read.
            out.extend(it.by_ref().map(|a| Word::Same(a.clone())));
        } else if matches!(a.as_str(), "--html" | "-o" | "--output") {
            if let Some(path) = it.next() {
                let name = format!("out-{tag}-{}", files.len());
                out.push(Word::File(name.clone()));
                files.push((path.clone(), name));
            }
        }
    }
    (out, files)
}

/// The script that runs wits there, or exits with ABSENT.
fn run_script(args: &[Word], sudo: bool, rm: bool, color: bool) -> String {
    let args: Vec<String> = args
        .iter()
        .map(|a| match a {
            Word::Same(s) => quote(s),
            Word::File(n) => format!("\"$d/{n}\""),
        })
        .collect();
    let mut s = format!("{}; [ -x \"$w\" ] || exit {ABSENT}; set -- \"$w\" {}", there(), args.join(" "));
    if color {
        s.push_str("; export CLICOLOR_FORCE=1");
    }
    if sudo {
        let env = if color { "env CLICOLOR_FORCE=1 " } else { "" };
        s.push_str(&format!("; [ \"$(id -u)\" = 0 ] || set -- sudo -n {env}\"$@\""));
    }
    s.push_str(if rm { "; \"$@\"; rc=$?; rm -f \"$w\"; exit $rc" } else { "; exec \"$@\"" });
    s
}

/// The release that runs on a machine, from its `uname -s` and `uname -m`.
fn target(os: &str, arch: &str) -> Result<String, String> {
    let arch = match arch {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return Err(format!("wits has no build for {os} on {arch}")),
    };
    match os {
        "Linux" => Ok(format!("{arch}-unknown-linux-musl")),
        "Darwin" => Ok(format!("{arch}-apple-darwin")),
        _ => Err(format!("wits runs on Linux and macOS, not on {os}")),
    }
}

/// The target this binary can stand in for on another machine: a static
/// Linux build, or macOS.
fn own_target() -> Option<String> {
    let arch = std::env::consts::ARCH;
    if cfg!(target_os = "macos") {
        Some(format!("{arch}-apple-darwin"))
    } else if cfg!(target_env = "musl") {
        Some(format!("{arch}-unknown-linux-musl"))
    } else {
        None
    }
}

fn quiet(cmd: &mut Command) -> Option<Vec<u8>> {
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn sha256(file: &std::path::Path) -> Option<String> {
    let out = quiet(Command::new("shasum").args(["-a", "256"]).arg(file))
        .or_else(|| quiet(Command::new("sha256sum").arg(file)))?;
    String::from_utf8_lossy(&out).split_whitespace().next().map(str::to_string)
}

/// A wits binary for `target`: the one named by WITS_REMOTE_BINARY, this
/// one, or the release, downloaded once and kept in the cache here.
fn binary(target: &str) -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("WITS_REMOTE_BINARY").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    if own_target().as_deref() == Some(target) {
        if let Ok(p) = std::env::current_exe() {
            return Ok(p);
        }
    }
    let other = "or set WITS_REMOTE_BINARY to a wits binary for that machine";
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or("no home directory to keep the download in")?
        .join("wits")
        .join(VERSION)
        .join(target);
    let bin = cache.join("wits");
    if bin.is_file() {
        return Ok(bin);
    }
    std::fs::create_dir_all(&cache).map_err(|e| format!("cannot create {}: {e}", cache.display()))?;
    let tgz = cache.join("wits.tar.gz");
    let bases = [format!("{RELEASES}/download/v{VERSION}"), format!("{RELEASES}/latest/download")];
    for (i, base) in bases.iter().enumerate() {
        let url = format!("{base}/wits-{target}.tar.gz");
        let curl = |url: &str| {
            let mut c = Command::new("curl");
            c.args(["-fsSL", "--connect-timeout", "15", url]);
            c
        };
        match curl(&url).arg("-o").arg(&tgz).stdin(Stdio::null()).stderr(Stdio::null()).status() {
            Ok(s) if s.success() => {}
            Ok(_) => continue,
            Err(e) => return Err(format!("downloading wits for {target} needs curl here ({e}), {other}")),
        }
        let want = quiet(&mut curl(&format!("{url}.sha256")))
            .and_then(|b| String::from_utf8_lossy(&b).split_whitespace().next().map(str::to_string));
        let got = sha256(&tgz);
        if want.is_none() || want != got {
            let _ = std::fs::remove_file(&tgz);
            return Err(format!("the download of {url} does not match its checksum; not using it"));
        }
        let ok = quiet(Command::new("tar").arg("-xzf").arg(&tgz).arg("-C").arg(&cache).arg("wits")).is_some();
        let _ = std::fs::remove_file(&tgz);
        if !ok || !bin.is_file() {
            return Err(format!("could not unpack {url}"));
        }
        if i > 0 {
            eprintln!("wits: wits {VERSION} has no release; using the latest one on that machine");
        }
        return Ok(bin);
    }
    Err(format!("could not download wits for {target} from {RELEASES}: check the network here, {other}"))
}

/// The line the scripts mark their answer with, so that whatever a login
/// script prints is not read as it.
fn marked(out: &[u8]) -> Option<String> {
    String::from_utf8_lossy(out).lines().find_map(|l| l.strip_prefix("wits-on: ").map(str::to_string))
}

/// Put wits on the machine.
fn install(via: &Via) -> Result<(), String> {
    let probe = format!(
        "{}; mkdir -p \"$d\" 2>/dev/null; if [ -d \"$d\" ] && [ -w \"$d\" ]; then ok=yes; else ok=no; fi; echo \"wits-on: $(uname -s) $(uname -m) $ok $d\"",
        there()
    );
    let (code, out) = via.read(&probe, None)?;
    let Some(line) = marked(&out) else {
        return Err(format!("could not run a command on {} (exit {code}), with: {}", via.name, via.shown()));
    };
    let f: Vec<&str> = line.splitn(4, ' ').collect();
    let [os, arch, ok, dir] = f.as_slice() else {
        return Err(format!("{} answered '{line}', which wits does not understand", via.name));
    };
    let target = target(os, arch)?;
    if *ok != "yes" {
        return Err(format!("cannot write to {dir} on {}, where wits would put its binary", via.name));
    }
    let bin = binary(&target)?;
    let size = std::fs::metadata(&bin).map_err(|e| format!("cannot read {}: {e}", bin.display()))?.len();
    eprintln!(
        "wits: not on {} yet: putting wits ({os} {arch}, {:.1} MB) in {dir}",
        via.name,
        size as f64 / 1e6
    );
    // Under a name of its own until it is whole. If it runs, older versions
    // go; if it does not, it goes.
    let put = format!(
        "{}; p=\"$w.$$.part\"; mkdir -p \"$d\" && cat > \"$p\" && chmod 755 \"$p\" && mv \"$p\" \"$w\" || {{ rm -f \"$p\"; exit 1; }}; \
         n=$(wc -c < \"$w\" | tr -d ' '); v=$(\"$w\" --version 2>&1 | head -n 1); \
         case \"$v\" in \"wits \"*) for f in \"$d\"/wits-*; do case \"$f\" in \"$w\"|*.part) ;; *) rm -f \"$f\" ;; esac; done ;; *) rm -f \"$w\" ;; esac; \
         echo \"wits-on: $n $v\"",
        there()
    );
    let file = File::open(&bin).map_err(|e| format!("cannot read {}: {e}", bin.display()))?;
    let (code, out) = via.read(&put, Some(file))?;
    let line = marked(&out).unwrap_or_default();
    let (got, version) = line.split_once(' ').unwrap_or((line.as_str(), ""));
    if code != 0 || got.parse::<u64>().ok() != Some(size) {
        return Err(format!(
            "could not copy wits to {} ({} of {size} bytes arrived, exit {code})",
            via.name,
            if got.is_empty() { "0" } else { got }
        ));
    }
    if !version.starts_with("wits ") {
        return Err(format!("wits does not run on {}: {version}", via.name));
    }
    Ok(())
}

/// Run wits there, with this terminal as its own. Lines on stderr that name
/// a file to bring back are left out: the file is not there for the user.
fn ask(via: &Via, script: &str, files: &[(String, String)]) -> Result<i32, String> {
    let mut cmd = via.command(script);
    if files.is_empty() {
        return Ok(cmd.status().map_err(|e| via.cannot(&e))?.code().unwrap_or(1));
    }
    let mut child = cmd.stderr(Stdio::piped()).spawn().map_err(|e| via.cannot(&e))?;
    if let Some(err) = child.stderr.take() {
        let mut r = BufReader::new(err);
        let mut line = Vec::new();
        while r.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
            let text = String::from_utf8_lossy(&line);
            if !files.iter().any(|(_, name)| text.contains(name.as_str())) {
                let _ = std::io::stderr().write_all(&line);
            }
            line.clear();
        }
    }
    Ok(child.wait().map_err(|e| via.cannot(&e))?.code().unwrap_or(1))
}

/// Bring a file wits wrote there to its path here. Files left by runs that
/// were cut short go after a day.
fn fetch(via: &Via, name: &str, path: &str) -> Result<bool, String> {
    let script = format!(
        "{}; f=\"$d/{name}\"; find \"$d\" -name 'out-*' -mtime +1 -exec rm -f {{}} + 2>/dev/null; [ -f \"$f\" ] || exit 3; cat \"$f\"; rc=$?; rm -f \"$f\"; exit $rc",
        there()
    );
    let (code, out) = via.read(&script, None)?;
    if code != 0 {
        return Ok(false);
    }
    std::fs::write(path, out).map_err(|e| format!("cannot write {path}: {e}"))?;
    Ok(true)
}

fn go(args: Vec<String>) -> Result<i32, String> {
    let (mut sudo, mut rm, mut by) = (false, false, None);
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next_if(|a| a.starts_with('-')) {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(0);
            }
            "--sudo" => sudo = true,
            "--rm" => rm = true,
            // It stands where the machine does: what follows is for wits there.
            "--via" => {
                by = Some(via(&it.next().ok_or("--via needs a command (try `wits on --help`)")?)?);
                break;
            }
            s => return Err(format!("unknown option '{s}' before the machine (try `wits on --help`)")),
        }
    }
    let to = match by {
        Some(v) => v,
        None => {
            let alone = unsafe { libc::isatty(0) == 0 && libc::isatty(2) == 0 };
            place(&it.next().ok_or("on which machine? (try `wits on --help`)")?, alone)?
        }
    };
    let rest: Vec<String> = it.collect();
    if let Some(c) = rest.first().filter(|c| matches!(c.as_str(), "diff" | "guide" | "mcp" | "on")) {
        return Err(format!("`wits {c}` works here, on this machine: run it without `on`"));
    }
    let tag = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
    );
    let (words, files) = rewrite(&rest, &tag);
    let color = unsafe { libc::isatty(1) } == 1 && std::env::var_os("NO_COLOR").is_none();
    let script = run_script(&words, sudo, rm, color);
    let mut code = ask(&to, &script, &files)?;
    if code == ABSENT {
        install(&to)?;
        code = ask(&to, &script, &files)?;
        if code == ABSENT {
            return Err(format!("wits is still not on {} after it was put there", to.name));
        }
    }
    for (path, name) in &files {
        if fetch(&to, name, path)? {
            let what = if path.ends_with(".json") { "" } else { "; open it in a browser" };
            eprintln!("wits: wrote {path}{what}");
        } else if code == 0 {
            return Err(format!("could not bring {path} back from {}", to.name));
        }
    }
    if code == 255 {
        eprintln!("wits: could not reach {} with: {}", to.name, to.shown());
    }
    Ok(code)
}

/// `wits on ...`: the exit status.
pub fn run(args: Vec<String>) -> i32 {
    if args.is_empty() {
        eprint!("{USAGE}");
        return 2;
    }
    match go(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wits: {e}");
            if e.contains("--help") {
                2
            } else {
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(w: &str) -> Vec<String> {
        place(w, false).unwrap_or_else(|e| panic!("{w}: {e}")).argv
    }

    #[test]
    fn places() {
        assert_eq!(argv("web-1"), ["ssh", "--", "web-1"]);
        assert_eq!(argv("deploy@10.0.0.5:2222"), ["ssh", "-p", "2222", "--", "deploy@10.0.0.5"]);
        assert_eq!(argv("ssh://ubuntu@db.internal"), ["ssh", "--", "ubuntu@db.internal"]);
        assert_eq!(argv("[fe80::1%en0]:22"), ["ssh", "-p", "22", "--", "fe80::1%en0"]);
        assert_eq!(argv("::1"), ["ssh", "--", "::1"]);

        assert_eq!(argv("gcp:web-1"), ["gcloud", "compute", "ssh", "web-1"]);
        assert_eq!(
            argv("gcp:alice@shop-prod/us-central1-a/web-1"),
            ["gcloud", "compute", "ssh", "alice@web-1", "--zone", "us-central1-a", "--project", "shop-prod"]
        );
        assert_eq!(place("gcp:web-1", false).unwrap().last, "--command=");

        // With nobody to ask, ssh is told not to ask.
        let to = |w: &str| place(w, true).unwrap();
        assert_eq!(to("web-1:2222").argv, ["ssh", "-o", "BatchMode=yes", "-p", "2222", "--", "web-1"]);
        assert_eq!(to("gcp:web-1").after, ["--", "-o", "BatchMode=yes"]);
        assert_eq!(to("azure:rg/vm").argv[7..], ["--", "-o", "BatchMode=yes"]);
        assert_eq!(to("aws:i-0123456789abcdef0").argv[..3], ["ssh", "-o", "BatchMode=yes"]);

        let aws = argv("aws:ec2-user@eu-west-1/i-0123456789abcdef0");
        assert_eq!(aws[0], "ssh");
        assert_eq!(
            aws[2],
            "ProxyCommand=sh -c \"aws ssm start-session --region eu-west-1 --target %h --document-name AWS-StartSSHSession --parameters 'portNumber=%p'\""
        );
        assert_eq!(aws[3..], ["--", "ec2-user@i-0123456789abcdef0"]);

        assert_eq!(
            argv("azure:azureuser@shop-rg/web-1"),
            ["az", "ssh", "vm", "--resource-group", "shop-rg", "--name", "web-1", "--local-user", "azureuser", "--"]
        );
    }

    /// A name is never read as an option or as a shell command, by ssh or
    /// by the tools in front of it.
    #[test]
    fn names_that_are_not_machines() {
        for w in [
            "",
            "-oProxyCommand=touch /tmp/x",
            "-v",
            "host;id",
            "host name",
            "$(id)",
            "user@-oProxyCommand=x",
            "-l root@host",
            "host:22x",
            "gcp:",
            "gcp:--project=x",
            "gcp:a/b/c/d",
            "gcp:zone/in stance",
            "aws:eu-west-1/--target",
            "azure:vm",
            "azure:rg/--name",
            "shop/api-7d9f",
        ] {
            assert!(place(w, false).is_err(), "{w:?} was taken for a machine");
        }
        assert!(place("shop/api", false).unwrap_err().contains("kubectl wits"));
    }

    #[test]
    fn via_words() {
        assert_eq!(words("fly ssh console -a app -C").unwrap(), ["fly", "ssh", "console", "-a", "app", "-C"]);
        assert_eq!(words("ssh -o 'ProxyCommand=nc %h %p'  host").unwrap(), ["ssh", "-o", "ProxyCommand=nc %h %p", "host"]);
        assert_eq!(words("a \"\" b").unwrap(), ["a", "", "b"]);
        assert!(words("sh -c 'oops").is_err());
        assert!(via("  ").is_err());
    }

    #[test]
    fn files_come_back() {
        let args: Vec<String> =
            ["run", "-o", "before.json", "--html", "run page.html", "--", "make", "-o", "x"].iter().map(|s| s.to_string()).collect();
        let (words, files) = rewrite(&args, "7");
        assert_eq!(files, [("before.json".into(), "out-7-0".into()), ("run page.html".into(), "out-7-1".to_string())]);
        // What follows `--` is the command's own.
        assert_eq!(words[words.len() - 3..], [Word::Same("make".into()), Word::Same("-o".into()), Word::Same("x".into())]);
        let s = run_script(&words, false, false, false);
        assert!(s.contains("set -- \"$w\" 'run' '-o' \"$d/out-7-0\" '--html' \"$d/out-7-1\" '--' 'make' '-o' 'x'; exec \"$@\""), "{s}");
    }

    /// The scripts are run by a real shell here: quoting holds, and a
    /// missing wits is told apart from one that fails.
    #[test]
    fn scripts_run() {
        let dir = std::env::temp_dir().join(format!("wits-on-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sh = |script: &str| {
            let mut c = Command::new("sh");
            c.arg("-c").arg(script).env("XDG_CACHE_HOME", &dir).stdin(Stdio::null()).stderr(Stdio::null());
            let out = c.output().unwrap();
            (out.status.code().unwrap(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let words = |a: &[&str]| a.iter().map(|s| Word::Same(s.to_string())).collect::<Vec<_>>();
        let odd = ["it's", "a b", "$HOME", "\"q\"", "-x", "*"];
        assert_eq!(sh(&run_script(&words(&odd), false, false, false)).0, ABSENT);

        // A stand-in for wits that prints its arguments, one per line.
        let bin = dir.join("wits").join(format!("wits-{VERSION}"));
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\necho \"color=$CLICOLOR_FORCE\"\nfor a in \"$@\"; do echo \"$a\"; done\nexit 7\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let (code, out) = sh(&run_script(&words(&odd), false, false, true));
        assert_eq!(code, 7);
        assert_eq!(out.lines().collect::<Vec<_>>(), ["color=1", "it's", "a b", "$HOME", "\"q\"", "-x", "*"]);
        // The whole thing as one word, as ssh hands it to the shell there.
        let (code, out) = sh(&format!("sh -c {}", quote(&run_script(&words(&odd), false, false, false))));
        assert_eq!((code, out.lines().nth(1)), (7, Some("it's")));

        // --rm: the status is still the command's, and wits is gone.
        assert_eq!(sh(&run_script(&words(&["x"]), false, true, false)).0, 7);
        assert!(!bin.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn targets() {
        assert_eq!(target("Linux", "x86_64").unwrap(), "x86_64-unknown-linux-musl");
        assert_eq!(target("Linux", "aarch64").unwrap(), "aarch64-unknown-linux-musl");
        assert_eq!(target("Darwin", "arm64").unwrap(), "aarch64-apple-darwin");
        assert!(target("FreeBSD", "amd64").is_err());
        assert!(target("Linux", "riscv64").is_err());
        assert_eq!(marked(b"Welcome to web-1\nwits-on: Linux x86_64 yes /home/a/.cache/wits\n").unwrap(), "Linux x86_64 yes /home/a/.cache/wits");
    }
}

use anyhow::{Context, Result, anyhow};
use language::{ByteContent, analyze_byte_content};
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::{
    env,
    fs::File,
    io::Read,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path,
    process::Command,
};
use url;

fn which(exe: &str) -> Result<path::PathBuf> {
    let path = env::var("PATH")?;
    for p in path.split(":") {
        if p.ends_with("/shell-integration") {
            continue;
        }
        let exe_path = path::PathBuf::from(p).join(exe);
        match exe_path.metadata() {
            Ok(m) => {
                if m.is_file() && m.permissions().mode() & 0o111 != 0 {
                    return Ok(exe_path);
                }
            }
            Err(_) => {}
        }
    }
    Err(anyhow!("err"))
}

fn positionals(args: &[OsString]) -> Vec<OsString> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let b = a.as_bytes();
        if b.starts_with(b"--") {
            // --xxx=value is self-contained; bare --xxx consumes the next arg
            if !b.contains(&b'=') {
                it.next();
            }
        } else if b.starts_with(b"-") {
            // -xxx: skip just this one
        } else {
            out.push(a.clone());
        }
    }
    out
}

fn is_binary(path: &str) -> Result<bool> {
    let mut buf = Vec::with_capacity(8000);
    File::open(path)?.take(8000).read_to_end(&mut buf)?;
    match analyze_byte_content(&buf) {
        ByteContent::Binary => Ok(false),
        _ => Ok(true),
    }
}

fn accepts(s: &str) -> bool {
    match url::Url::parse(&s) {
        Ok(_) => true,
        Err(_) => {
            match is_binary(s) {
                Ok(v) => {
                    if !v {
                        return false;
                    }
                }
                Err(_) => {}
            }
            path::PathBuf::from(s).is_file()
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let open = which("open")?;
    let zetty = which("zetty")?;

    // Check if there are any swiches
    if args.is_empty() || args.iter().any(|a| a.to_string_lossy().starts_with('-')) {
        return passthrough(open, &args);
    }

    // Pass to zetty
    let positionals = positionals(&args);
    if positionals.iter().all(|a| accepts(&a.to_string_lossy())) {
        passthrough(zetty, &args)
    } else {
        passthrough(open, &args)
    }
}

fn passthrough(exe: path::PathBuf, args: &[OsString]) -> Result<()> {
    let err = Command::new(exe).args(args).exec();
    Err(err).context("failed to exec open")
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

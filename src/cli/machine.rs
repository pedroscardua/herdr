use std::io;
use std::path::PathBuf;

use crate::machine::{ssh, Catalog, Machine};

pub(super) fn run(args: &[String]) -> io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("add") => add(&args[1..]),
        Some("list") if args.len() <= 2 => {
            let catalog = Catalog::load()?;
            if args.get(1).is_some_and(|s| s == "--json") {
                println!("{}", serde_json::to_string_pretty(&catalog.machines)?);
            } else if args.len() == 1 {
                for m in catalog.machines {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        m.id,
                        m.label,
                        m.target,
                        m.session,
                        if m.enabled { "enabled" } else { "disabled" }
                    );
                }
            } else {
                return Ok(usage());
            }
            Ok(0)
        }
        Some("status") => status(&args[1..]),
        Some("reconnect") if args.len() == 2 => {
            let catalog = Catalog::load()?;
            let machine = catalog.find(&args[1])?;
            ssh::check(machine, true)?;
            crate::client::verify_machine(machine)?;
            println!(
                "{} is reachable. Open clients retry automatically.",
                machine.label
            );
            Ok(0)
        }
        Some("rename") if args.len() == 4 && args[2] == "--label" => {
            Catalog::update(|c| {
                let id = c.find(&args[1])?.id.clone();
                if let Some(m) = c.machines.iter_mut().find(|m| m.id == id) {
                    m.label = args[3].clone();
                }
                Ok(())
            })?;
            Ok(0)
        }
        Some("enable" | "disable" | "remove") if args.len() == 2 => {
            Catalog::update(|c| {
                let id = c.find(&args[1])?.id.clone();
                if args[0] == "remove" {
                    c.machines.retain(|m| m.id != id);
                } else if let Some(m) = c.machines.iter_mut().find(|m| m.id == id) {
                    m.enabled = args[0] == "enable";
                }
                Ok(())
            })?;
            println!("Machine {}. Remote processes were left running.", args[0]);
            Ok(0)
        }
        Some("--help" | "-h" | "help") | None => {
            usage();
            Ok(0)
        }
        _ => Ok(usage()),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AddOptions {
    target: String,
    label: String,
    session: String,
    install: bool,
    binary: Option<PathBuf>,
}

fn parse_add(args: &[String]) -> io::Result<AddOptions> {
    let mut target = None;
    let mut label = None;
    let mut session = "motionharvest".to_string();
    let mut install = false;
    let mut binary = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--label" | "--remote-session" | "--binary" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| io::Error::other("missing option value"))?
                    .clone();
                match args[index].as_str() {
                    "--label" => label = Some(value),
                    "--remote-session" => session = value,
                    _ => binary = Some(PathBuf::from(value)),
                }
                index += 2;
            }
            "--install" => {
                install = true;
                index += 1;
            }
            value if !value.starts_with('-') && target.is_none() => {
                target = Some(value.to_string());
                index += 1;
            }
            _ => {
                return Err(io::Error::other(
                    "invalid machine add arguments; see machine --help",
                ))
            }
        }
    }
    let target = target.ok_or_else(|| io::Error::other("missing SSH target"))?;
    let label = label.unwrap_or_else(|| target.clone());
    Ok(AddOptions {
        target,
        label,
        session,
        install,
        binary,
    })
}

fn add(args: &[String]) -> io::Result<i32> {
    let options = parse_add(args)?;
    let mut machine = Machine::new(options.label, options.target, options.session)?;
    // Repeating the same setup updates its existing ID instead of duplicating rows.
    if let Ok(existing) = Catalog::load()?.find(&machine.label) {
        if existing.target != machine.target || existing.session != machine.session {
            return Err(io::Error::other(
                "label already belongs to a different host/session; choose another --label",
            ));
        }
        machine.id = existing.id.clone();
    }
    machine.protocol = ssh::setup(&machine, options.install, options.binary.as_deref())?;
    crate::client::verify_machine(&machine)?;
    Catalog::update(|c| {
        if let Some(existing) = c.machines.iter_mut().find(|m| m.id == machine.id) {
            *existing = machine.clone();
        } else {
            c.machines.push(machine.clone());
        }
        Ok(())
    })?;
    println!(
        "Saved {} ({}). Remote session {} is ready. Open clients connect automatically.",
        machine.label, machine.target, machine.session
    );
    Ok(0)
}

fn status(args: &[String]) -> io::Result<i32> {
    let mut selector = None;
    let mut json = false;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else if !arg.starts_with('-') && selector.is_none() {
            selector = Some(arg.as_str());
        } else {
            return Ok(usage());
        }
    }
    let catalog = Catalog::load()?;
    let machines: Vec<_> = if let Some(selector) = selector {
        vec![catalog.find(selector)?]
    } else {
        catalog.machines.iter().collect()
    };
    let mut results = Vec::new();
    let mut failed = false;
    for m in machines {
        let error = ssh::check(m, false).err().map(|e| e.to_string());
        failed |= error.is_some();
        if !json {
            println!(
                "{}\t{}{}",
                m.label,
                if error.is_none() {
                    "reachable"
                } else {
                    "error"
                },
                error.as_ref().map(|e| format!("\t{e}")).unwrap_or_default()
            );
        }
        results.push(serde_json::json!({"id":m.id,"label":m.label,"status":if error.is_none(){"reachable"}else{"error"},"error":error}));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    }
    Ok(i32::from(failed))
}

fn usage() -> i32 {
    eprintln!("herdr machine commands:\n  add <SSH_TARGET> [--label TEXT] [--remote-session NAME] [--install] [--binary PATH]\n  list [--json]\n  status [LABEL_OR_ID] [--json]\n  reconnect <LABEL_OR_ID>\n  rename <LABEL_OR_ID> --label TEXT\n  enable|disable|remove <LABEL_OR_ID>\n\nSetup uses an isolated motionharvest binary and session on the host.\n--install explicitly permits installation; --binary selects a local file for the REMOTE platform.\nWithout --binary, installation downloads the matching motionharvest release and verifies SHA-256.\nSSH authentication uses your normal SSH config and agent.\nSwitch machines by clicking the bottom machine bar or pressing Ctrl+Alt+M.");
    2
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_options_can_precede_target_and_session_is_isolated() {
        let args = ["--label", "VM 223", "VM223", "--install"].map(String::from);
        let o = parse_add(&args).unwrap();
        assert_eq!(o.label, "VM 223");
        assert_eq!(o.target, "VM223");
        assert_eq!(o.session, "motionharvest");
        assert!(o.install);
        assert!(parse_add(&["VM223".into(), "--label".into()]).is_err());
    }
}

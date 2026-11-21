use std::env;
use std::ffi::OsString;
use std::fs::{self, Permissions};
use std::io::{self, ErrorKind, IsTerminal};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;
use tracing::info;

use crate::certs::Ca;
use crate::config::Config;
use crate::error::Error;
use crate::result::Result;
use crate::settings::Settings;

const BLOCK: &str = include_str!("../hosts");
const SYSTEMD: &str = include_str!("../traces.service");
const LAUNCHD: &str = include_str!("../traces.plist");
const START: &str = "# traces start";
const END: &str = "# traces end";
pub const PRIVILEGED: &str = "privileged";

#[derive(Deserialize)]
pub struct Platforms {
    macos: System,
    linux: System,
}

#[derive(Deserialize)]
struct System {
    hosts_file: PathBuf,
    refresh: Vec<Vec<String>>,
    binary: PathBuf,
    service: Service,
    trust: Vec<TrustStore>,
}

#[derive(Deserialize)]
struct Service {
    file: PathBuf,
    after_install: Vec<Vec<String>>,
    start: Vec<Vec<String>>,
    before_remove: Vec<Vec<String>>,
    after_remove: Vec<Vec<String>>,
}

#[derive(Deserialize)]
struct TrustStore {
    present: PathBuf,
    check: Vec<String>,
    install: Vec<Vec<String>>,
    remove: Vec<Vec<String>>,
}

/// Manage the installation, starting, and uninstallation of the traces service.
pub fn manage(command: &str) -> Result<()> {
    // Ensure the command is run with sudo privileges.
    if env::var_os("SUDO_USER").is_some() {
        return Err(Error::WithoutSudo);
    }
    // Load or create the CA certificate for TLS connections.
    let ca = Ca::load_or_create(&Config::load()?.ca_dir()?)?.cert_path;
    let mut arguments: Vec<OsString> = vec![PRIVILEGED.into(), command.into(), ca.into()];
    // If the command is "install", gather additional environment variables for the privileged command.
    if command == "install" { // If the command is "install", gather additional environment variables.
        Settings::from_env_file()?;
        // Load the environment variables from the .env file.
        let variable = |name: &'static str| env::var_os(name).ok_or(Error::MissingEnv(name.into()));
        // Append the necessary environment variables to the arguments for the privileged command.
        arguments.extend([
            variable("USER")?,
            variable("HOME")?,
            env::current_dir()?.into(),
            env::current_exe()?.into(),
        ]);
    }
    // Execute the privileged command using sudo.
    let mut sudo = Command::new("sudo");
    // Add the current executable and the prepared arguments to the sudo command.
    if env::var_os("SUDO_ASKPASS").is_some() {
        sudo.arg("-A");
    } else if !io::stdin().is_terminal() {
        sudo.arg("-n");
    }
    // Run the sudo command and check its exit status.
    let status = sudo.arg(env::current_exe()?).args(arguments).status();
    // Map any errors from running the sudo command to the appropriate error type.
    let status = status.map_err(|source| Error::Command { command: "sudo".into(), source })?;
    // Check if the sudo command was successful and return the appropriate result.
    if status.success() { Ok(()) } else { Err(Error::Elevation) }
}

/// Execute the privileged command with the provided arguments.
/// This function handles the installation, starting, and uninstallation of the traces service.
pub fn privileged() -> Result<()> {
    // Collect the arguments passed to the privileged command, 
    // Skipping the first two (the executable and the command itself).
    let arguments: Vec<String> = env::args().skip(2).collect();
    // Load the system-specific configuration for the current platform.
    let platforms = Config::load()?.system;
    // Determine the system and template based on the current platform.
    let (system, template) =
        if cfg!(target_os = "macos") { (&platforms.macos, LAUNCHD) } else { (&platforms.linux, SYSTEMD) };
    // Match the command and execute the corresponding action.
    match arguments.as_slice() {
        [command, ca, user, home, workdir, exe] if command == "install" => {
            let values = [("{user}", user.as_str()), ("{home}", home.as_str()), ("{workdir}", workdir.as_str())];
            install(system, template, Path::new(ca), &values, Path::new(exe))?;
            info!("traces is installed. Execute `traces start` to start the server or reboot the system.");
            Ok(())
        }
        [command, ca] if command == "start" => {
            // Start the traces service using the system-specific start command.
            run_all(&system.service.start, Path::new(ca))
        }
        [command, ca] if command == "uninstall" => {
            // Uninstall the traces service from the system.
            uninstall(system, Path::new(ca))
        }
        _ => Err(Error::Usage),
    }
}

fn install(system: &System, template: &str, ca: &Path, values: &[(&'static str, &str)], exe: &Path) -> Result<()> {
    // Validate the provided values to ensure they do not contain unsafe characters.
    if let Some((name, _)) = values.iter().find(|(_, value)| value.contains(['\n', '<', '>', '&', '"'])) {
        return Err(Error::Unsafe(name));
    }
    // Replace placeholders in the template with the actual values.
    let unit = template.replace("{binary}", &system.binary.to_string_lossy());
    // Apply the user-provided values to the template.
    let unit = values.iter().fold(unit, |text, (key, val)| text.replace(key, val));
    // Stage the new binary for installation.
    let staged = system.binary.with_extension("new");
    // Ensure the parent directory of the binary exists.
    if let Some(folder) = system.binary.parent() {
        fs::create_dir_all(folder).map_err(at(folder))?;
    }
    // Copy the new binary to the staged location.
    fs::copy(exe, &staged).map_err(at(&staged))?;
    // Set the appropriate permissions for the staged binary.
    fs::set_permissions(&staged, Permissions::from_mode(0o755))?;
    // Move the staged binary to its final location.
    fs::rename(&staged, &system.binary).map_err(at(&system.binary))?;
    // Locate the trust store and install it if necessary.
    let store = system.trust.iter().find(|store| store.present.exists()).ok_or(Error::NoTrustStore)?;
    // Check if the trust store is already installed.
    if !execute(&store.check, ca)? {
        // Install the trust store if it is not present.
        run_all(&store.install, ca)?;
    }
    // Update the hosts file with the new block.
    write(&system.hosts_file, &(without_block(&read(&system.hosts_file)?) + BLOCK))?;
    // Refresh the system to apply changes.
    run_all(&system.refresh, ca)?;
    // Write the service unit file.
    write(&system.service.file, &unit)?;
    // Execute any post-installation steps for the service.
    run_all(&system.service.after_install, ca)
}

/// Uninstall the service and clean up associated files and configurations.
fn uninstall(system: &System, ca: &Path) -> Result<()> {
    // Helper function to remove a file if it exists.
    let gone = |path: &Path| match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(at(path)(error)),
        _ => Ok(()),
    };
    // Helper function to update the hosts file by removing the block.
    let hosts = || read(&system.hosts_file).and_then(|hosts| write(&system.hosts_file, &without_block(&hosts)));
    
    // Locate the trust store if it exists.
    let store = system.trust.iter().find(|store| store.present.exists());
    
    // Execute the uninstallation steps in order.
    let steps = [
        // Execute any pre-removal steps for the service.
        run_all(&system.service.before_remove, ca),
        // Remove the service unit file.
        gone(&system.service.file),
        // Execute any post-removal steps for the service.
        run_all(&system.service.after_remove, ca),
        // Remove the trust store if it exists.
        store.map_or(Ok(()), |store| run_all(&store.remove, ca)),
        // Update the hosts file to remove the block.
        hosts(),
        // Refresh the system to apply changes.
        run_all(&system.refresh, ca),
        // Remove the service binary.
        gone(&system.binary),
    ];
    // Collect any failures from the uninstallation steps.
    let failures: Vec<String> = steps.into_iter().filter_map(Result::err).map(|error| error.to_string()).collect();
    // Return an error if any of the steps failed, otherwise return Ok.
    if failures.is_empty() { Ok(()) } else { Err(Error::Failed(failures.join("; "))) }
}

/// Remove the block of text between the START and END markers from the hosts file.
fn without_block(hosts: &str) -> String {
    // Split the hosts file into lines for processing.
    let lines: Vec<&str> = hosts.lines().collect();
    // Determine the range of lines to remove based on the START and END markers.
    let range =
        lines.iter().position(|line| line.trim() == START).zip(lines.iter().position(|line| line.trim() == END));
    // Filter out the lines within the determined range and reconstruct the hosts file.
    lines
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !range.is_some_and(|(start, end)| (start..=end).contains(index)))
        .map(|(_, line)| format!("{line}\n"))
        .collect()
}

/// Execute all commands in the given list, returning an error if any command fails.
fn run_all(commands: &[Vec<String>], ca: &Path) -> Result<()> {
    for command in commands {
        if !execute(command, ca)? {
            return Err(Error::Failed(command.join(" ")));
        }
    }
    Ok(())
}

/// Execute a single command, returning true if it succeeds and false otherwise.
fn execute(words: &[String], ca: &Path) -> Result<bool> {
    // Replace the "{ca}" placeholder in the command words with the actual CA path.
    let words: Vec<String> = words.iter().map(|word| word.replace("{ca}", &ca.to_string_lossy())).collect();
    // Split the command into the program and its arguments.
    let (program, arguments) = words.split_first().ok_or(Error::Usage)?;
    // Execute the command and capture its exit status.
    let status = Command::new(program).args(arguments).stdin(Stdio::null()).status();
    Ok(status.map_err(|source| Error::Command { command: words.join(" "), source })?.success())
}

fn at(path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::File { path, source }
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(at(path))
}

fn write(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text).map_err(at(path))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{BLOCK, END, LAUNCHD, START, SYSTEMD, Service, System, TrustStore, install, uninstall, without_block};
    use crate::config::Config;
    use crate::error::Error;

    #[test]
    fn removes_only_a_complete_block_and_keeps_every_other_line() {
        let hosts = format!("127.0.0.1 localhost\n{START}\n127.0.0.1 api.anthropic.com\n{END}\n10.0.0.5 gateway\n");
        assert_eq!(without_block(&hosts), "127.0.0.1 localhost\n10.0.0.5 gateway\n");
        let broken = format!("127.0.0.1 localhost\n{START}\n10.0.0.5 gateway");
        assert_eq!(without_block(&broken), format!("127.0.0.1 localhost\n{START}\n10.0.0.5 gateway\n"));
        assert!(BLOCK.starts_with(START) && BLOCK.trim_end().ends_with(END));
        assert!(BLOCK.lines().filter(|line| !line.starts_with('#')).all(|line| line.starts_with("127.0.0.1 ")));
        let platforms = Config::load().unwrap().system;
        assert!(platforms.macos.service.file.ends_with("local.traces.plist") && LAUNCHD.contains("local.traces"));
        assert!(platforms.linux.service.file.ends_with("traces.service") && SYSTEMD.contains("CAP_NET_BIND_SERVICE"));
    }

    #[test]
    fn installs_everything_once_and_uninstalls_it_again() {
        let dir = tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        let text = |name: &str| fs::read_to_string(path(name)).unwrap();
        let words = |words: &[&str]| words.iter().map(|word| word.to_string()).collect::<Vec<_>>();
        fs::write(path("hosts"), "127.0.0.1 localhost\n").unwrap();
        fs::write(path("ca.crt"), "synthetic CA").unwrap();
        fs::write(path("traces-build"), "synthetic binary").unwrap();
        let anchor = path("anchor.crt").to_string_lossy().into_owned();
        let store = TrustStore {
            present: dir.path().to_path_buf(),
            check: words(&["cmp", "-s", "{ca}", &anchor]),
            install: vec![words(&["cp", "{ca}", &anchor])],
            remove: vec![words(&["rm", "-f", &anchor])],
        };
        let service = Service {
            file: path("traces.service"),
            after_install: vec![words(&["true"])],
            start: vec![words(&["true"])],
            before_remove: vec![words(&["true"])],
            after_remove: vec![words(&["true"])],
        };
        let binary = path("bin/traces");
        let system =
            System { hosts_file: path("hosts"), refresh: vec![words(&["true"])], binary, service, trust: vec![store] };
        let values = [("{user}", "alice"), ("{home}", "/home/alice"), ("{workdir}", "/home/alice/traces")];
        for _ in 0..2 {
            install(&system, SYSTEMD, &path("ca.crt"), &values, &path("traces-build")).unwrap();
        }
        assert_eq!(text("hosts"), format!("127.0.0.1 localhost\n{BLOCK}"));
        assert_eq!(
            (text("anchor.crt"), text("bin/traces")),
            ("synthetic CA".to_string(), "synthetic binary".to_string())
        );
        let unit = text("traces.service");
        assert!(unit.contains("User=alice\n") && unit.contains("WorkingDirectory=/home/alice/traces\n"), "{unit}");
        assert!(unit.contains(&format!("ExecStart={}\n", path("bin/traces").display())), "{unit}");
        uninstall(&system, &path("ca.crt")).unwrap();
        assert_eq!(text("hosts"), "127.0.0.1 localhost\n");
        assert!(["anchor.crt", "traces.service", "bin/traces"].iter().all(|name| !path(name).exists()));
        let unsafe_values = [("{user}", "alice"), ("{home}", "/home/alice"), ("{workdir}", "/tmp/<x>")];
        let refused = install(&system, LAUNCHD, Path::new("ca.crt"), &unsafe_values, &path("traces-build"));
        assert!(matches!(refused, Err(Error::Unsafe("{workdir}"))));
    }
}

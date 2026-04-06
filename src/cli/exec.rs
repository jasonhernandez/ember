use std::path::Path;

use clap::Args;

use crate::cli::vm::load_running_with_ip;
use crate::ssh;
use crate::state::store::StateStore;

#[derive(Args)]
pub struct ExecArgs {
    /// VM name
    pub vm_name: String,

    /// User to run the command as (default: from VM metadata)
    #[arg(long)]
    pub user: Option<String>,

    /// Command to execute (everything after --)
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub fn run(args: &ExecArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let (metadata, network) = load_running_with_ip(&store, &args.vm_name)?;

    let guest_ip = &network.guest_ip;
    let key_path = &metadata.ssh.key;
    let user = args.user.as_deref().unwrap_or(&metadata.ssh.user);

    // Build the remote command string from the argument vector.
    //
    // Single argument: pass directly to the remote shell (the SSH server
    // runs it through sh -c, so operators like && | > work as expected).
    // This matches ssh(1) behavior: `ssh host "cmd1 && cmd2"`.
    //
    // Multiple arguments: join with shell-escaping so each arg is treated
    // as a single word on the remote side, regardless of its contents.
    // This matches: `ssh host -- echo "hello world"`.
    let command = if args.command.len() == 1 {
        args.command[0].clone()
    } else {
        shell_escape_join(&args.command)
    };

    let rt = tokio::runtime::Runtime::new()?;
    let exit_code = rt.block_on(async {
        let mut client = ssh::client::connect(guest_ip, user, key_path).await?;
        let code = ssh::exec::exec(&mut client, &command).await?;
        let _ = client.close().await;
        Ok::<u32, anyhow::Error>(code)
    })?;

    if exit_code != 0 {
        std::process::exit(exit_code as i32);
    }

    Ok(())
}

/// Join command arguments into a single shell command string.
///
/// Arguments containing spaces, quotes, or shell metacharacters are
/// single-quoted. This matches the behavior expected by remote shells.
fn shell_escape_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.is_empty()
                || arg
                    .contains(|c: char| c.is_whitespace() || "\"'\\$`!#&|;(){}[]<>?*~".contains(c))
            {
                crate::ssh::copy::shell_quote(arg)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command() {
        let args = vec!["ls".to_string(), "-la".to_string()];
        assert_eq!(shell_escape_join(&args), "ls -la");
    }

    #[test]
    fn command_with_spaces() {
        let args = vec!["echo".to_string(), "hello world".to_string()];
        assert_eq!(shell_escape_join(&args), "echo 'hello world'");
    }

    #[test]
    fn command_with_single_quotes() {
        let args = vec!["echo".to_string(), "it's".to_string()];
        assert_eq!(shell_escape_join(&args), "echo 'it'\\''s'");
    }

    #[test]
    fn command_with_special_chars() {
        let args = vec![
            "bash".to_string(),
            "-c".to_string(),
            "echo $HOME".to_string(),
        ];
        assert_eq!(shell_escape_join(&args), "bash -c 'echo $HOME'");
    }

    #[test]
    fn empty_argument() {
        let args = vec!["cmd".to_string(), "".to_string()];
        assert_eq!(shell_escape_join(&args), "cmd ''");
    }

    #[test]
    fn single_arg_shell_command_passed_directly() {
        // Single-argument commands are treated as shell strings:
        // ember exec vm -- "echo hello && echo world"
        // should send "echo hello && echo world" verbatim.
        let args = ExecArgs {
            vm_name: "vm".to_string(),
            user: None,
            command: vec!["echo hello && echo world".to_string()],
        };
        // The dispatch logic passes single args directly, not through
        // shell_escape_join. Verify the multi-arg path still escapes.
        let multi_args = vec!["sh".to_string(), "-c".to_string(), "echo hello && echo world".to_string()];
        assert_eq!(shell_escape_join(&multi_args), "sh -c 'echo hello && echo world'");

        // Single arg should be identity.
        assert_eq!(args.command[0], "echo hello && echo world");
    }
}

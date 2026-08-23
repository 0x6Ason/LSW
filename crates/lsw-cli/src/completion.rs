// SPDX-License-Identifier: GPL-3.0-or-later

//! Dependency-free shell completion generation.

use std::ffi::OsString;

const COMMANDS: &str = "bench clone compact completion config create daemon diagnose doctor exec help hibernate image inspect install license list logs media memory path plan prepare profile pull push remove resume run seed share shell show shutdown start status stop suspend sync trim use user version view";

pub(super) fn command(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let [shell] = arguments else {
        return Err("usage: lsw completion <bash|zsh|fish|powershell>".into());
    };
    let shell = shell
        .to_str()
        .ok_or("completion shell must be valid UTF-8")?;
    match shell {
        "bash" => print!("{}", bash()),
        "zsh" => print!("{}", zsh()),
        "fish" => print!("{}", fish()),
        "powershell" | "pwsh" => print!("{}", powershell()),
        _ => return Err("usage: lsw completion <bash|zsh|fish|powershell>".into()),
    }
    Ok(())
}

fn bash() -> String {
    format!(
        r#"_lsw() {{
    local cur prev commands instances
    COMPREPLY=()
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    commands="{COMMANDS}"
    if (( COMP_CWORD == 1 )); then
        COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
        return
    fi
    case "${{COMP_WORDS[1]}}" in
        completion) COMPREPLY=( $(compgen -W "bash zsh fish powershell" -- "$cur") ); return ;;
        path) COMPREPLY=( $(compgen -W "--windows -w --unix -u" -- "$cur") ); return ;;
    esac
    instances="$(lsw list 2>/dev/null | awk 'NR > 1 {{print $1}}')"
    COMPREPLY=( $(compgen -W "$instances" -- "$cur") )
}}
complete -F _lsw lsw
"#
    )
}

fn zsh() -> String {
    format!(
        r#"#compdef lsw
_lsw() {{
  local -a commands instances
  commands=({COMMANDS})
  if (( CURRENT == 2 )); then
    _describe 'command' commands
    return
  fi
  case $words[2] in
    completion) _values 'shell' bash zsh fish powershell; return ;;
    path) _values 'direction' --windows -w --unix -u; return ;;
  esac
  instances=(${{(f)"$(lsw list 2>/dev/null | awk 'NR > 1 {{print $1}}')"}})
  _describe 'instance' instances
}}
compdef _lsw lsw
"#
    )
}

fn fish() -> String {
    let mut output = String::from(
        "function __lsw_instances\n    lsw list 2>/dev/null | string match -r '^[^\\t]+\\t' | string replace -r '\\t.*$' '' | string match -rv '^NAME$'\nend\ncomplete -c lsw -f\n",
    );
    for command in COMMANDS.split_whitespace() {
        output.push_str(&format!(
            "complete -c lsw -n '__fish_use_subcommand' -a '{command}'\n"
        ));
    }
    output.push_str(
        "complete -c lsw -n 'not __fish_use_subcommand' -a '(__lsw_instances)'\ncomplete -c lsw -n '__fish_seen_subcommand_from completion' -a 'bash zsh fish powershell'\ncomplete -c lsw -n '__fish_seen_subcommand_from path' -a '--windows -w --unix -u'\n",
    );
    output
}

fn powershell() -> String {
    format!(
        r#"Register-ArgumentCompleter -Native -CommandName lsw -ScriptBlock {{
    param($wordToComplete, $commandAst, $cursorPosition)
    $commands = '{COMMANDS}'.Split(' ')
    $elements = @($commandAst.CommandElements)
    if ($elements.Count -le 2) {{
        $candidates = $commands
    }} elseif ($elements[1].Value -eq 'completion') {{
        $candidates = @('bash', 'zsh', 'fish', 'powershell')
    }} elseif ($elements[1].Value -eq 'path') {{
        $candidates = @('--windows', '-w', '--unix', '-u')
    }} else {{
        $candidates = @(lsw list 2>$null | Select-Object -Skip 1 | ForEach-Object {{ ($_ -split '\s+')[0] }})
    }}
    $candidates | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }}
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_completion_contains_commands_and_dynamic_instances() {
        for script in [bash(), zsh(), fish(), powershell()] {
            assert!(script.contains("install"));
            assert!(script.contains("sync"));
            assert!(script.contains("lsw list"));
        }
    }
}

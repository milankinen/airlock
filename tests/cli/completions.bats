#!/usr/bin/env bats

load helpers

# Shell completions: the completion script, the completion requests and the
# install into the startup file of the shell.

setup() {
    setup_temp_dir
}

# Load the bash completion script of airlock in a new bash and complete the
# words in the arguments. The last argument is the word to complete. Sets
# $status, $output and $lines.
bash_complete() {
    run env XDG_DATA_HOME="$TEST_DATA_HOME" HOME="$TEST_TEMP_DIR" NO_COLOR=1 \
        bash -c '
            airlock="$1"
            shift
            eval "$("$airlock" completions script bash)"
            COMP_WORDS=("$@")
            COMP_CWORD=$(($# - 1))
            COMP_TYPE=9
            _clap_complete_airlock airlock "${COMP_WORDS[COMP_CWORD]}" ""
            printf "%s\n" "${COMPREPLY[@]}"
        ' _ "$AIRLOCK" airlock "$@"
}

# Make a template with the name $1 in the data directory.
make_template() {
    mkdir -p "$TEST_DATA_HOME/airlock/templates/$1"
    touch "$TEST_DATA_HOME/airlock/templates/$1/airlock.toml"
}

# Test that the bash completion script completes the commands and the names
# of the templates. The names are not in the command line definition, so
# airlock must read them when the user completes.
#   1. Make two templates
#   2. Load the script in bash and complete a command name
#   3. Complete a template name with no prefix and with a prefix
@test "bash completion script completes commands and template names" {
    make_template python
    make_template node

    bash_complete te
    assert_success
    [[ "$output" == "templates" ]]

    bash_complete templates info ""
    assert_success
    assert_output_contains "python"
    assert_output_contains "node"

    bash_complete templates remove p
    assert_success
    [[ "$output" == "python" ]]
}

# Test that a completion request does not make the data directory. The
# shell sends a request for each TAB, and it must not change files.
#   1. Complete a template name with no data directory
#   2. Check that there are no candidates and no data directory
@test "completion without data directory makes no files" {
    bash_complete templates info x
    assert_success
    [[ -z "$output" ]]
    [[ ! -e "$TEST_DATA_HOME/airlock" ]]
}

# Test that the completion request completes the ids of the sandboxes, with
# the project directory as the help of each id.
#   1. Make the sandbox of the project
#   2. Send a fish completion request for a sandbox id
#   3. Check that the id and the project directory are in the output
@test "completion request completes sandbox ids with project directory" {
    make_box
    local id="${BOX_DIR##*/}"

    AIRLOCK_COMPLETE=fish run_airlock -- airlock sandboxes info ""
    assert_success
    assert_output_contains "$id"$'\t'"$PWD"
}

# Test that install adds the completion line to the startup file of the
# shell in $SHELL one time, and keeps the old text of the file.
#   1. Write a .bashrc and run install with SHELL=/bin/bash
#   2. Check that the old text and the line are in the file
#   3. Run install again and check that the file has the line one time
@test "completions install adds line to bashrc one time" {
    echo "export FOO=1" >.bashrc

    SHELL=/bin/bash XDG_CONFIG_HOME="" run_airlock completions install
    assert_success
    assert_output_contains "Added completions to"
    grep -qx "export FOO=1" .bashrc
    grep -qF 'eval "$(airlock completions script bash)"' .bashrc

    SHELL=/bin/bash XDG_CONFIG_HOME="" run_airlock completions install
    assert_success
    assert_output_contains "already"
    [[ "$(grep -cF 'airlock completions script' .bashrc)" -eq 1 ]]
}

# Test that install does not add the line when the startup file loads the
# completions in a form that the user wrote. A line for another shell does
# not count.
#   1. Write a .bashrc that loads the zsh script, and run install bash
#   2. Check that install added the line
#   3. Write a .bashrc with a custom line for the bash script
#   4. Run install bash and check that the file did not change
@test "completions install keeps custom completion line" {
    echo 'eval "$(airlock completions script zsh)"' >.bashrc
    SHELL=/bin/bash XDG_CONFIG_HOME="" run_airlock completions install
    assert_success
    assert_output_contains "Added completions to"

    printf '%s\n' 'source <(/opt/bin/airlock  completions script bash)' >.bashrc
    local before
    before="$(cat .bashrc)"
    SHELL=/bin/bash XDG_CONFIG_HOME="" run_airlock completions install
    assert_success
    assert_output_contains "already"
    [[ "$(cat .bashrc)" == "$before" ]]
}

# Test that install for zsh and fish writes the startup file of that shell.
# Zsh reads its startup files from $ZDOTDIR, and fish from
# $XDG_CONFIG_HOME, when they are set.
#   1. Run install zsh with ZDOTDIR set and check its .zshrc
#   2. Run install fish without XDG_CONFIG_HOME and check the file in home
#   3. Run install fish with XDG_CONFIG_HOME and check the file in it
@test "completions install writes zsh and fish startup files" {
    mkdir zdot
    ZDOTDIR="$PWD/zdot" run_airlock completions install zsh
    assert_success
    grep -qF 'airlock completions script zsh' zdot/.zshrc

    XDG_CONFIG_HOME="" run_airlock completions install fish
    assert_success
    grep -qF 'airlock completions script fish | source' \
        .config/fish/completions/airlock.fish

    XDG_CONFIG_HOME="$PWD/xdg" run_airlock completions install fish
    assert_success
    grep -qF 'airlock completions script fish | source' \
        xdg/fish/completions/airlock.fish
}

# Test that install fails when $SHELL is a shell without completion support,
# and that it writes no file.
#   1. Run install with SHELL=/bin/tcsh
#   2. Check the error and that there is no startup file
@test "completions install with unknown shell fails" {
    SHELL=/bin/tcsh run_airlock completions install
    assert_failure 1
    assert_output_contains 'unknown shell "/bin/tcsh"'
    [[ ! -e .bashrc && ! -e .zshrc ]]
}

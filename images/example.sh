#!/bin/bash
# An example image file: Rust, the Helix editor, the PostgreSQL client,
# Claude Code and a CLAUDE.md that tells it about the VM. Copy it to ~/.config/sendit/images/NAME.sh to
# define the image NAME, e.g. images/rust.sh for `sendit provision rust`, and
# the scripts it calls from provision-scripts/ to
# ~/.config/sendit/provision-scripts/.
#
# It runs like the scripts it calls: as the VM's user, in a login shell from
# the home directory, with sudo. It also runs with `bash -e`, so a command
# that fails, a script it calls included, stops the build.

sendit-provision rust.sh
# Built with Rust, so it runs after it.
sendit-provision helix.sh
sudo apt-get -y install --no-install-recommends postgresql-client
sendit-provision claude-code.sh
sendit-provision claude-md.sh

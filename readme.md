# Send it

Send it is a tool that utilizes macOS 27 native technologies to provide light and fast VMs that developers can use to run e.g. their Coding Clients inside. It uses a Debian Linux base image which is provisioned once and then re-used for any projects that might use a VM (i.e. it is copied to the project and then started from there, two projects don't use the same VM). The project directory is then mounted inside the VM as well as some other configurable paths. The .git path of projects should be "overwritten" by an empty mount so the VM cannot see the git history. This can be disabled with a startup flag.

It should be possible to configure how many CPUs and how much RAM the VM can utilize. The disk of the VM can grow dynamically based on how much storage it actually needs. The size should not be fixed. For the networking layer, macOS native technologies should be used.

During provisioning, we run a base script which updates the Debian and installs a couple of basics like git, sets the hostname to "sendit" and presents a cool ascii art that reads "SEND IT".
